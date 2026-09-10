//! The chunk-of-one component rasterizer shared by Lambda and local mode.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;

use crate::component_svg::{build_component_svg, filter_count};
use crate::compositor::RgbaImage;
use crate::contract::{PrepareManifest, RasterEvent, RasterResult};
use crate::error::RasterError;
use crate::import::{import_ffdec_symbol_with_visibility, AuthoredColorTransform};
use crate::raster::{downsample_component_to_output_grid, encode_rgba8, render_svg_bounded};
use crate::storage::{Sink, Source};
use crate::telemetry::{log_raster_profile, sha256_hex, RasterStats, RasterTimings};

pub const COMPONENT_RASTER_SPACE_RASTER: &str = "raster";
pub const COMPONENT_RASTER_SPACE_OUTPUT: &str = "output";

fn elapsed_ms(started: Instant) -> f64 {
    started.elapsed().as_secs_f64() * 1000.0
}

/// Python's `_frame_canvas_sizes` (shared with the composer).
pub fn frame_canvas_sizes(
    viewbox: &[f64],
    raster_size: i64,
    output_size: i64,
) -> Result<([i64; 2], [i64; 2]), RasterError> {
    if viewbox.len() != 4 || viewbox[2] <= 0.0 || viewbox[3] <= 0.0 {
        return Err(RasterError::invalid(
            "Prepare manifest has no usable shared viewbox",
        ));
    }
    if raster_size <= 0 || output_size <= 0 || output_size > raster_size {
        return Err(RasterError::invalid(
            "Output size must be positive and cannot exceed raster size",
        ));
    }
    let pixel_scale = raster_size as f64 / viewbox[2].max(viewbox[3]);
    let raster_canvas = [
        crate::svg::py_round(viewbox[2] * pixel_scale).max(1),
        crate::svg::py_round(viewbox[3] * pixel_scale).max(1),
    ];
    if output_size == raster_size {
        return Ok((raster_canvas, raster_canvas));
    }
    let scale = output_size as f64 / raster_canvas[0].max(raster_canvas[1]) as f64;
    let output_canvas = [
        crate::svg::py_round(raster_canvas[0] as f64 * scale).max(1),
        crate::svg::py_round(raster_canvas[1] as f64 * scale).max(1),
    ];
    Ok((raster_canvas, output_canvas))
}

/// Extract one gzip tar member in memory (mirrors `_extract_member`).
fn extract_member(archive_bytes: &[u8], member: &str) -> Result<(Vec<u8>, u64), RasterError> {
    let decoder = flate2::read::GzDecoder::new(archive_bytes);
    let mut archive = tar::Archive::new(decoder);
    let mut candidates: Vec<(String, Vec<u8>)> = Vec::new();
    for entry in archive.entries().map_err(|error| RasterError::Bundle {
        key: String::new(),
        member: member.to_string(),
        message: format!("cannot read archive: {error}"),
    })? {
        let mut entry = entry.map_err(|error| RasterError::Bundle {
            key: String::new(),
            member: member.to_string(),
            message: format!("cannot read entry: {error}"),
        })?;
        let path = entry
            .path()
            .map_err(|error| RasterError::Bundle {
                key: String::new(),
                member: member.to_string(),
                message: format!("cannot read entry path: {error}"),
            })?
            .to_string_lossy()
            .to_string();
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut entry, &mut bytes).map_err(|error| {
            RasterError::Bundle {
                key: String::new(),
                member: member.to_string(),
                message: format!("cannot read entry body: {error}"),
            }
        })?;
        candidates.push((path, bytes));
    }
    let selected = candidates.iter().find(|(path, _)| path == member);
    match selected {
        Some((_, bytes)) => Ok((bytes.clone(), bytes.len() as u64)),
        None => Err(RasterError::Bundle {
            key: String::new(),
            member: member.to_string(),
            message: "missing member".to_string(),
        }),
    }
}

/// Rasterize exactly one unique placed component state, matching the known
/// deterministic S3 keys so retries are idempotent.
pub async fn run_raster_task(
    event: &RasterEvent,
    source: &dyn Source,
    sink: &dyn Sink,
) -> Result<RasterResult, RasterError> {
    let started = Instant::now();

    // ---- manifest -----------------------------------------------------------
    let manifest_started = Instant::now();
    let manifest_value = source.read_json(&event.manifest_key).await?;
    let prepared: PrepareManifest = serde_json::from_value(manifest_value)?;
    let manifest_ms = elapsed_ms(manifest_started);

    if prepared.job_id != event.job_id {
        return Err(RasterError::invalid(
            "Prepare manifest belongs to another job",
        ));
    }
    let task_index = event.task_index;
    let tasks = &prepared.component_tasks;
    if task_index < 0 || task_index >= tasks.len() as i64 {
        return Err(RasterError::invalid(format!(
            "Component task index {task_index} is out of range"
        )));
    }
    let task = tasks[task_index as usize].clone();

    let viewbox: [f64; 4] = {
        let values = &prepared.viewbox;
        if values.len() != 4 {
            return Err(RasterError::invalid(
                "Prepare manifest has no usable shared viewbox",
            ));
        }
        [values[0], values[1], values[2], values[3]]
    };
    let settings = &prepared.settings;
    let raster_size = settings.raster_size;
    let output_size = settings.output_size;
    let (raster_canvas, output_canvas) =
        frame_canvas_sizes(&prepared.viewbox, raster_size, output_size)?;
    let component_raster_space = prepared
        .component_raster_space
        .clone()
        .unwrap_or_else(|| COMPONENT_RASTER_SPACE_RASTER.to_string());
    if component_raster_space != COMPONENT_RASTER_SPACE_RASTER
        && component_raster_space != COMPONENT_RASTER_SPACE_OUTPUT
    {
        return Err(RasterError::invalid(format!(
            "Unsupported component raster space {component_raster_space:?}"
        )));
    }

    let zoom = settings.zoom;
    // Selected SVG rasterizer: the manifest carries the render job's choice
    // (hydrated by the launcher from the SQS payload / CLI). The local-raster
    // CLI can force a backend per invocation for A/B comparisons.
    let raster_backend_name = event
        .raster_backend
        .clone()
        .unwrap_or_else(|| settings.raster_backend.clone());
    let mut raster_backend =
        crate::raster::RenderBackend::parse(&raster_backend_name).ok_or_else(|| {
            RasterError::invalid(format!(
                "Unsupported render backend {raster_backend_name:?}"
            ))
        })?;
    let symbol_key = task.symbol_key.clone();
    let part = prepared.parts.get(&symbol_key).cloned().ok_or_else(|| {
        RasterError::invalid(format!(
            "Component task references unknown part {symbol_key}"
        ))
    })?;
    let color_rules: HashMap<String, (String, String)> = part
        .color_rules
        .iter()
        .map(|(name, rule)| {
            let rule = if rule.len() >= 2 {
                (rule[0].clone(), rule[1].clone())
            } else {
                (rule.first().cloned().unwrap_or_default(), String::new())
            };
            (name.to_lowercase(), rule)
        })
        .collect();
    let placement_color_values = crate::storage::parse_placement_key_string(&part.placement_colors);
    let placement_colors: HashMap<(i64, i64), AuthoredColorTransform> = placement_color_values
        .into_iter()
        .map(|(key, value)| (key, value.into()))
        .collect();
    let matrix: [f64; 6] = {
        let values = &task.matrix;
        if values.len() != 6 {
            return Err(RasterError::invalid(format!(
                "Component task {} has no usable matrix",
                task.task_id
            )));
        }
        [
            values[0], values[1], values[2], values[3], values[4], values[5],
        ]
    };
    let darken = task.darken;
    let bundle_key = task.bundle_key.clone();
    let member = task.member.clone();
    let task_id = task.task_id.clone();
    if let Some(bounds) = &task.raster_bounds {
        bounds.validate()?;
    }

    // ---- content-addressed cache --------------------------------------------
    // Appearance-independent parts (no color/placement customization) can be
    // reused verbatim across jobs and characters: the key is computed from
    // manifest + task fields only, so a hit skips bundle download, import,
    // SVG build, resvg, downsample, and encode.
    let cache_key = if prepared.cache.components && crate::cache::is_no_cc(&part) {
        match &task.state_signature {
            Some(signature) => Some(crate::cache::cache_key(
                signature,
                matrix,
                darken,
                task.layer_index,
                &task.layer_name,
                viewbox,
                raster_size,
                output_size,
                &component_raster_space,
                zoom,
                raster_backend.to_str(),
                &part,
            )?),
            None => None,
        }
    } else {
        None
    };
    let cache_key = cache_key
        .map(|key| crate::cache::bounds_key(&key, &task.raster_bounds))
        .transpose()?;
    let mut cache_hit = false;
    if let Some(key) = &cache_key {
        if let Some((cached_meta, cached_png)) = crate::cache::read_cache(source, key).await? {
            let mut result = RasterResult {
                task_id: task_id.clone(),
                layers: Vec::new(),
                empty: cached_meta.empty,
                x: cached_meta.x,
                y: cached_meta.y,
                width: cached_meta.width,
                height: cached_meta.height,
                sha256: cached_meta.sha256.clone(),
                bytes: cached_meta.bytes,
                png_key: None,
                input_bytes: cached_meta.input_bytes,
                svg_bytes: cached_meta.svg_bytes,
                filter_count: cached_meta.filter_count,
                component_raster_space: component_raster_space.clone(),
                canvas_width: if component_raster_space == COMPONENT_RASTER_SPACE_OUTPUT {
                    output_canvas[0]
                } else {
                    raster_canvas[0]
                },
                canvas_height: if component_raster_space == COMPONENT_RASTER_SPACE_OUTPUT {
                    output_canvas[1]
                } else {
                    raster_canvas[1]
                },
                state_signature: task.state_signature.clone(),
                symbol_key: symbol_key.clone(),
                layer_name: task.layer_name.clone(),
                result_key: String::new(),
                render_backend: raster_backend.to_str().to_string(),
                rasterize_ms: 0.0,
                crop_ms: 0.0,
                downsample_ms: 0.0,
            };
            let png_key = match &event.benchmark_output_prefix {
                Some(prefix) => format!("{prefix}/rasters/{task_id}.png"),
                None => format!("jobs/{}/component/rasters/{task_id}.png", event.job_id),
            };
            if let Some(png) = cached_png {
                sink.put(&png_key, "image/png", &png).await?;
                result.png_key = Some(png_key);
            }
            let result_key = match &event.benchmark_output_prefix {
                Some(prefix) => format!("{prefix}/results/{task_id}.json"),
                None => format!("jobs/{}/component/results/{task_id}.json", event.job_id),
            };
            result.result_key = result_key.clone();
            sink.put(
                &result_key,
                "application/json",
                &serde_json::to_vec(&result)?,
            )
            .await?;
            cache_hit = true;

            let total_ms = elapsed_ms(started);
            let stats = RasterStats {
                job_id: event.job_id.clone(),
                task_id,
                symbol_key,
                layer_name: result.layer_name.clone(),
                empty: result.empty,
                svg_bytes: cached_meta.svg_bytes,
                input_bytes: cached_meta.input_bytes,
                filter_count: cached_meta.filter_count,
                raster_pixel_count: if result.empty {
                    0
                } else {
                    (result.width * result.height) as u64
                },
                component_raster_space,
                raster_canvas: (raster_canvas[0], raster_canvas[1]),
                output_canvas: (output_canvas[0], output_canvas[1]),
                cache_enabled: prepared.cache.components,
                cache_hit,
                raster_backend,
                timings: RasterTimings {
                    manifest_ms,
                    ..RasterTimings::default()
                },
                total_ms,
            };
            log_raster_profile(&stats);
            return Ok(result);
        }
    }

    // ---- bundle -------------------------------------------------------------
    let bundle_started = Instant::now();
    let bundle_bytes = source
        .fetch(task.svg_key.as_deref().unwrap_or(&bundle_key))
        .await?;
    let bundle_download_ms = elapsed_ms(bundle_started);

    let extract_started = Instant::now();
    let (state_svg, input_bytes) = if task.svg_key.is_some() {
        let expected = task
            .state_signature
            .as_deref()
            .ok_or_else(|| RasterError::invalid("direct SVG task has no checksum"))?;
        if sha256_hex(&bundle_bytes) != expected {
            return Err(RasterError::invalid("direct SVG checksum mismatch"));
        }
        let size = bundle_bytes.len() as u64;
        (bundle_bytes, size)
    } else {
        extract_member(&bundle_bytes, &member)?
    };
    let extract_ms = elapsed_ms(extract_started);
    let state_svg = String::from_utf8(state_svg)
        .map_err(|_| RasterError::Svg("state svg is not valid utf-8".to_string()))?;

    // ---- import + assembly --------------------------------------------------
    let import_started = Instant::now();
    let imported = import_ffdec_symbol_with_visibility(
        &symbol_key,
        &state_svg,
        zoom,
        &color_rules,
        &part.root_class,
        &placement_colors,
        part.character_id,
        &part.hand_visibility,
        match task.layer_name.as_str() { "gauntlet_front" => Some("fronthand"), "gauntlet_back" => Some("backhand"), _ => None },
    )?;
    let import_ms = elapsed_ms(import_started);

    let svg_started = Instant::now();
    let placed_key = format!("{:02}_{}", task.layer_index, task.layer_name);
    let fields = &prepared.fields;
    let all_color_rules: Vec<(String, String)> = prepared
        .all_color_rules
        .iter()
        .map(|rule| {
            let first = rule.first().cloned().unwrap_or_default();
            let second = rule.get(1).cloned().unwrap_or_default();
            (first, second)
        })
        .collect();
    let mut component = build_component_svg(
        &imported,
        matrix,
        darken,
        &placed_key,
        &task.layer_name,
        viewbox,
        raster_size,
        fields,
        &all_color_rules,
    );
    if component.visible && crate::visibility::proven_invisible(&component.root) {
        component.visible = false;
    }
    let svg_bytes = crate::svg::serialize(&crate::svg::Document {
        root: component.root.clone(),
        namespaces: component.namespaces.clone(),
    })
    .into_bytes();
    if let Ok(dump_dir) = std::env::var("AQW_DUMP_COMPONENT_SVG") {
        let _ = std::fs::create_dir_all(&dump_dir);
        let path = PathBuf::from(dump_dir).join(format!("task-{task_index:02}.svg"));
        let _ = std::fs::write(path, &svg_bytes);
    }
    let svg_byte_count = svg_bytes.len() as u64;
    // The private SWF Add extension belongs to the patched resvg backend.
    // Experimental ThorVG jobs use resvg for these components as well.
    let has_additive = svg_bytes.windows(7).any(|s| s == b"aqw-add");
    if has_additive { raster_backend = crate::raster::RenderBackend::Resvg; }
    let filter_count_value = filter_count(&component.root);
    let svg_build_ms = elapsed_ms(svg_started);

    let [mut left, mut top, right, bottom] = component.page;
    let mut page_width = (right - left).max(1) as u32;
    let mut page_height = (bottom - top).max(1) as u32;

    let mut result = RasterResult {
        task_id: task_id.clone(),
        layers: Vec::new(),
        empty: true,
        x: 0,
        y: 0,
        width: 0,
        height: 0,
        sha256: None,
        bytes: None,
        png_key: None,
        input_bytes,
        svg_bytes: svg_byte_count,
        filter_count: filter_count_value,
        component_raster_space: component_raster_space.clone(),
        canvas_width: if component_raster_space == COMPONENT_RASTER_SPACE_OUTPUT {
            output_canvas[0]
        } else {
            raster_canvas[0]
        },
        canvas_height: if component_raster_space == COMPONENT_RASTER_SPACE_OUTPUT {
            output_canvas[1]
        } else {
            raster_canvas[1]
        },
        state_signature: task.state_signature.clone(),
        symbol_key: symbol_key.clone(),
        layer_name: task.layer_name.clone(),
        result_key: String::new(),
        render_backend: raster_backend.to_str().to_string(),
        rasterize_ms: 0.0,
        crop_ms: 0.0,
        downsample_ms: 0.0,
    };

    let mut timings = RasterTimings {
        manifest_ms,
        bundle_download_ms,
        extract_ms,
        import_ms,
        svg_build_ms,
        ..RasterTimings::default()
    };

    // The final PNG bytes are kept so the content-addressed cache can be
    // populated after a miss (only for appearance-independent parts).
    let mut cached_png_bytes: Option<Vec<u8>> = None;

    // Keep Add passes separate until frame composition, so they see the armor,
    // other equipment and background. Process/encode one layer at a time.
    let layered = if component.visible && has_additive {
        let tree = resvg::usvg::Tree::from_data(&svg_bytes, &resvg::usvg::Options::default())
            .map_err(|e| RasterError::Raster(e.to_string()))?;
        let layers = resvg::layers::plan(&tree);
        if layers.iter().any(|l| l.additive) {
            if layers.len() > 128 { return Err(RasterError::Raster("component additive layer limit exceeded".into())); }
            let mut total_bytes = 0u64;
            for layer in &layers {
                let raster_started = Instant::now();
                let mut pixels = resvg::tiny_skia::Pixmap::new(page_width, page_height)
                    .ok_or_else(|| RasterError::Raster("additive layer allocation".into()))?;
                resvg::layers::render(layer, resvg::tiny_skia::Transform::identity(), &mut pixels.as_mut());
                let mut data = pixels.take();
                crate::raster::demultiply_u8(&mut data);
                let image = RgbaImage::new(page_width, page_height, data);
                timings.rasterize_ms += elapsed_ms(raster_started);
                let crop_started = Instant::now();
                let Some(bbox) = image.alpha_bbox(page_width, page_height) else { continue; };
                let cropped = image.crop(bbox);
                timings.crop_ms += elapsed_ms(crop_started);
                let x = left + bbox.0 as i64;
                let y = top + bbox.1 as i64;
                let downsample_started = Instant::now();
                let output = if component_raster_space == COMPONENT_RASTER_SPACE_OUTPUT && output_canvas != raster_canvas {
                    downsample_component_to_output_grid(&cropped,x,y,(raster_canvas[0],raster_canvas[1]),(output_canvas[0],output_canvas[1]))
                } else { Some((cropped,x,y)) };
                timings.downsample_ms += elapsed_ms(downsample_started);
                let Some((image,x,y)) = output else { continue; };
                let bytes = encode_rgba8(image.width,image.height,&image.pixels)?;
                let layer_index = result.layers.len();
                let key = match &event.benchmark_output_prefix {
                    Some(prefix) => format!("{prefix}/rasters/{task_id}-layer-{layer_index}.png"),
                    None => format!("jobs/{}/component/rasters/{task_id}-layer-{layer_index}.png",event.job_id),
                };
                let upload_started = Instant::now();
                sink.put(&key,"image/png",&bytes).await?;
                timings.upload_ms += elapsed_ms(upload_started);
                total_bytes += bytes.len() as u64;
                result.layers.push(crate::contract::RasterLayer { png_key:key,sha256:sha256_hex(&bytes),x,y,
                    blend_mode:if layer.additive {"add"} else {"normal"} });
            }
            result.empty = result.layers.is_empty();
            result.width = page_width.into(); result.height = page_height.into();
            result.bytes = Some(total_bytes);
            result.rasterize_ms = timings.rasterize_ms;
            result.crop_ms = timings.crop_ms;
            result.downsample_ms = timings.downsample_ms;
            true
        } else { false }
    } else { false };

    if component.visible && !layered {
        let rasterize_started = Instant::now();
        let hint = task
            .raster_bounds
            .as_ref()
            .and_then(|h| h.bounds)
            .map(|bounds| {
                let b = crate::import::transformed_bounds(bounds, matrix);
                let scale = raster_size as f64 / viewbox[2].max(viewbox[3]);
                [
                    (b[0] - viewbox[0]) * scale - left as f64,
                    (b[1] - viewbox[1]) * scale - top as f64,
                    b[2] * scale,
                    b[3] * scale,
                ]
            });
        let (rendered, offset, region_reason) =
            render_svg_bounded(&svg_bytes, (page_width, page_height), raster_backend, hint)?;
        eprintln!(
            "{}",
            serde_json::json!({"event":"component_raster_region", "job_id":event.job_id,
            "task_id":task_id, "policy":crate::region::POLICY, "reason":region_reason,
            "original_page_width":page_width, "original_page_height":page_height,
            "allocated_width":rendered.width, "allocated_height":rendered.height,
            "original_page_pixels":page_width as u64 * page_height as u64,
            "allocated_pixels":rendered.width as u64 * rendered.height as u64})
        );
        left += offset[0] as i64;
        top += offset[1] as i64;
        page_width = rendered.width;
        page_height = rendered.height;
        timings.rasterize_ms = elapsed_ms(rasterize_started);
        result.rasterize_ms = crate::telemetry::rounded2(timings.rasterize_ms);

        let crop_started = Instant::now();
        let bbox = rendered.alpha_bbox(page_width, page_height);
        timings.crop_ms = elapsed_ms(crop_started);
        result.crop_ms = crate::telemetry::rounded2(timings.crop_ms);

        if let Some(bbox) = bbox {
            let (crop_left, crop_top, _, _) = bbox;
            let cropped = rendered.crop(bbox);
            let mut component_x = left + crop_left as i64;
            let mut component_y = top + crop_top as i64;

            let mut output_image: Option<RgbaImage> = None;
            if component_raster_space == COMPONENT_RASTER_SPACE_OUTPUT
                && output_canvas != raster_canvas
            {
                let downsample_started = Instant::now();
                let scaled = downsample_component_to_output_grid(
                    &cropped,
                    component_x,
                    component_y,
                    (raster_canvas[0], raster_canvas[1]),
                    (output_canvas[0], output_canvas[1]),
                );
                timings.downsample_ms = elapsed_ms(downsample_started);
                result.downsample_ms = crate::telemetry::rounded2(timings.downsample_ms);

                if let Some((image, x, y)) = scaled {
                    output_image = Some(image);
                    component_x = x;
                    component_y = y;
                }
            } else {
                output_image = Some(cropped);
            }

            if let Some(image) = output_image {
                let png_bytes = encode_rgba8(image.width, image.height, &image.pixels)?;
                let sha = sha256_hex(&png_bytes);
                result.empty = false;
                result.x = component_x;
                result.y = component_y;
                result.width = image.width as i64;
                result.height = image.height as i64;
                result.sha256 = Some(sha);
                result.bytes = Some(png_bytes.len() as u64);
                if cache_key.is_some() {
                    cached_png_bytes = Some(png_bytes.clone());
                }

                let png_key = match &event.benchmark_output_prefix {
                    Some(prefix) => format!("{prefix}/rasters/{task_id}.png"),
                    None => format!("jobs/{}/component/rasters/{task_id}.png", event.job_id),
                };
                let upload_started = Instant::now();
                sink.put(&png_key, "image/png", &png_bytes).await?;
                timings.upload_ms = elapsed_ms(upload_started);
                result.png_key = Some(png_key);
            }
        }
    }

    // ---- populate the cache on a miss ---------------------------------------
    if let Some(key) = cache_key.as_ref().filter(|_| !has_additive) {
        let meta = crate::cache::CacheMeta {
            empty: result.empty,
            x: result.x,
            y: result.y,
            width: result.width,
            height: result.height,
            sha256: result.sha256.clone(),
            bytes: result.bytes,
            input_bytes,
            svg_bytes: svg_byte_count,
            filter_count: filter_count_value,
        };
        // Best-effort: a failed cache write must never fail or change a render.
        if let Err(error) =
            crate::cache::write_cache(sink, key, &meta, cached_png_bytes.as_deref()).await
        {
            eprintln!("component raster cache write failed: {error}");
        }
    }

    let result_key = match &event.benchmark_output_prefix {
        Some(prefix) => format!("{prefix}/results/{task_id}.json"),
        None => format!("jobs/{}/component/results/{task_id}.json", event.job_id),
    };
    result.result_key = result_key.clone();
    let upload_started = Instant::now();
    sink.put(
        &result_key,
        "application/json",
        &serde_json::to_vec(&result)?,
    )
    .await?;
    timings.upload_ms += elapsed_ms(upload_started);

    let total_ms = elapsed_ms(started);
    let stats = RasterStats {
        job_id: event.job_id.clone(),
        task_id,
        symbol_key,
        layer_name: result.layer_name.clone(),
        empty: result.empty,
        svg_bytes: svg_byte_count,
        input_bytes,
        filter_count: filter_count_value,
        raster_pixel_count: if result.empty {
            0
        } else {
            (result.width * result.height) as u64
        },
        component_raster_space,
        raster_canvas: (raster_canvas[0], raster_canvas[1]),
        output_canvas: (output_canvas[0], output_canvas[1]),
        cache_enabled: prepared.cache.components,
        cache_hit,
        raster_backend,
        timings,
        total_ms,
    };
    log_raster_profile(&stats);
    Ok(result)
}
