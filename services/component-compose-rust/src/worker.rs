//! The chunk composer shared by the Lambda runtime and the local mode.
//!
//! Contract parity with the Python worker (`stages/component_compose.py`):
//! one process starts once, shared components are decoded once, and each
//! globally unique full-frame recipe assigned to it is composed, encoded with
//! the pinned cwebp, uploaded, and expanded back into logical frame records.
//! A missing task or missing PNG fails the whole chunk.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::PathBuf;
use std::time::Instant;

use futures::stream::{self, StreamExt};

use crate::compositor::{frame_canvas_sizes, Canvas, RgbaImage};
use crate::contract::{
    BatchIndex, BatchManifest, ComponentFrame, ComponentResult, ComposeEvent, ComposeOutput,
    FrameRecord, PrepareManifest,
};
use crate::encode::encode_webp;
use crate::error::ComposeError;
use crate::png;
use crate::storage::{Sink, Source};
use crate::telemetry::{sha256_hex, ChunkStats, Timings};

pub struct ComposeOptions {
    pub download_concurrency: usize,
    pub cwebp: PathBuf,
    pub scratch_dir: PathBuf,
    /// Keep lossless pre-WebP frame PNGs beneath this directory (local mode);
    /// Lambda deletes them after encoding.
    pub retain_png_dir: Option<PathBuf>,
}

fn elapsed_ms(started: Instant) -> f64 {
    started.elapsed().as_secs_f64() * 1000.0
}

fn invalid(message: impl Into<String>) -> ComposeError {
    ComposeError::invalid(message)
}

fn stored_component_results(
    event: &ComposeEvent,
    manifest: serde_json::Value,
) -> Result<Vec<ComponentResult>, ComposeError> {
    if manifest["schema_version"] != 1
        || manifest["job_id"] != event.job_id
        || manifest["prepare_manifest_key"] != event.manifest_key
    {
        return Err(invalid("Component results manifest mismatch"));
    }
    Ok(serde_json::from_value(manifest["component_results"].clone())?)
}

fn missing_preview(missing: &[String]) -> String {
    let mut preview = missing
        .iter()
        .take(5)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    if missing.len() > 5 {
        preview.push_str(", ...");
    }
    preview
}

/// Fetch every unique referenced PNG with bounded concurrency. Returns
/// `(task_id, bytes, byte_count)` pairs.
async fn fetch_components(
    task_ids: &[String],
    results: &HashMap<String, ComponentResult>,
    source: &dyn Source,
    concurrency: usize,
) -> Result<Vec<(String, Vec<u8>, u64)>, ComposeError> {
    let fetched = stream::iter(task_ids.iter().cloned())
        .map(|task_id| {
            let results = results.clone();
            async move {
                let result = results.get(&task_id).ok_or_else(|| {
                    ComposeError::invalid(format!("task {task_id} has no result"))
                })?;
                let png_key = result.png_key.clone().ok_or_else(|| {
                    ComposeError::invalid(format!("task {task_id} has no png_key"))
                })?;
                let bytes = source
                    .fetch_bytes(&png_key, result.sha256.as_deref())
                    .await?;
                let byte_count = bytes.len() as u64;
                Ok((task_id, bytes, byte_count))
            }
        })
        .buffer_unordered(concurrency.max(1))
        .collect::<Vec<Result<(String, Vec<u8>, u64), ComposeError>>>()
        .await;
    fetched.into_iter().collect()
}

/// Legacy raster-space fallback: shrink one composed full-raster canvas to
/// the delivered output grid with premultiplied-alpha Lanczos filtering.
///
/// This branch exists only for pre-v19 manifests (`component_raster_space`
/// absent or `"raster"`). It is intentionally not pixel-identical to
/// Pillow's multistep `reducing_gap=3.0` shrink (documented limitation); the
/// v19 output-grid path never reaches it.
fn downsample_canvas(canvas: Canvas, output_size: i64) -> Result<Canvas, ComposeError> {
    use fast_image_resize as fir;

    let longest = canvas.width.max(canvas.height) as i64;
    if longest == output_size {
        return Ok(canvas);
    }
    if longest < output_size {
        return Err(invalid("Output size cannot exceed the raster frame size"));
    }
    let scale = output_size as f64 / longest as f64;
    let target_width = crate::compositor::py_round(canvas.width as f64 * scale).max(1) as u32;
    let target_height = crate::compositor::py_round(canvas.height as f64 * scale).max(1) as u32;

    let source_image = fir::images::Image::from_vec_u8(
        canvas.width,
        canvas.height,
        canvas.pixels,
        fir::PixelType::U8x4,
    )
    .map_err(|error| invalid(format!("downsample input: {error}")))?;
    let mut destination =
        fir::images::Image::new(target_width, target_height, source_image.pixel_type());
    let options = fir::ResizeOptions::new()
        .resize_alg(fir::ResizeAlg::Convolution(fir::FilterType::Lanczos3))
        .use_alpha(true);
    fast_image_resize::Resizer::new()
        .resize(&source_image, &mut destination, &options)
        .map_err(|error| invalid(format!("downsample: {error}")))?;
    let pixels = destination.buffer().to_vec();
    Ok(Canvas::from_pixels(target_width, target_height, pixels))
}

fn frame_duration(frame: &ComponentFrame, durations: &[i64], frame_number: i64) -> i64 {
    frame
        .duration_ms
        .filter(|&value| value != 0)
        .unwrap_or_else(|| durations[(frame_number - 1) as usize])
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CompositionWork {
    canonical_frame: i64,
    layers: Vec<String>,
    logical_frames: Vec<i64>,
}

struct PendingUpload {
    frame_number: i64,
    key: String,
    bytes: Vec<u8>,
}

/// Validate the global recipe partition and select the unique compositions
/// addressed by this batch. Old frame-range events remain supported during
/// rollout and by the local benchmarking CLI.
fn select_compositions(
    prepared: &PrepareManifest,
    batch: &BatchIndex,
) -> Result<Vec<CompositionWork>, ComposeError> {
    let frame_count = prepared.frame_count;
    if prepared.component_frames.len() != frame_count as usize {
        return Err(invalid(
            "Component frame count does not match the prepared frame count",
        ));
    }
    if prepared.frame_durations.len() != frame_count as usize {
        return Err(invalid(
            "Frame duration count does not match the prepared frame count",
        ));
    }
    for (index, frame) in prepared.component_frames.iter().enumerate() {
        if frame.number != index as i64 + 1 {
            return Err(invalid("Component frames are not contiguous"));
        }
    }

    let composition_range = match (batch.composition_start, batch.composition_end) {
        (Some(start), Some(end)) => Some((start, end)),
        (None, None) => None,
        _ => return Err(invalid("Incomplete component composition range")),
    };
    let frame_range = match (batch.frame_start, batch.frame_end) {
        (Some(start), Some(end)) => Some((start, end)),
        (None, None) => None,
        _ => return Err(invalid("Incomplete component frame range")),
    };
    if composition_range.is_some() && frame_range.is_some() {
        return Err(invalid("Compose batch mixes composition and frame ranges"));
    }

    if let Some((start, end)) = composition_range {
        let compositions = &prepared.component_compositions;
        if start > end || end >= compositions.len() {
            return Err(invalid(format!(
                "Invalid component composition batch {start}-{end} for {} compositions",
                compositions.len()
            )));
        }
        let mut claimed = vec![false; frame_count as usize];
        for composition in compositions {
            if composition.logical_frames.is_empty()
                || !composition
                    .logical_frames
                    .contains(&composition.canonical_frame)
            {
                return Err(invalid(
                    "Component composition has no canonical logical frame",
                ));
            }
            for &number in &composition.logical_frames {
                if number < 1 || number > frame_count {
                    return Err(invalid("Component composition frame is out of range"));
                }
                let index = number as usize - 1;
                if std::mem::replace(&mut claimed[index], true) {
                    return Err(invalid("Logical frame appears in multiple compositions"));
                }
                if prepared.component_frames[index].layers != composition.layers {
                    return Err(invalid(
                        "Component composition layers differ from its logical frame",
                    ));
                }
            }
        }
        if claimed.iter().any(|claimed| !claimed) {
            return Err(invalid(
                "Component compositions do not cover every logical frame",
            ));
        }
        return Ok(compositions[start..=end]
            .iter()
            .map(|composition| CompositionWork {
                canonical_frame: composition.canonical_frame,
                layers: composition.layers.clone(),
                logical_frames: composition.logical_frames.clone(),
            })
            .collect());
    }

    let (start, end) = frame_range.ok_or_else(|| invalid("Compose batch has no range"))?;
    if start < 1 || end > frame_count || start > end {
        return Err(invalid(format!(
            "Invalid component compose batch {start}-{end} for {frame_count} frames"
        )));
    }
    Ok(prepared.component_frames[start as usize - 1..end as usize]
        .iter()
        .map(|frame| CompositionWork {
            canonical_frame: frame.number,
            layers: frame.layers.clone(),
            logical_frames: vec![frame.number],
        })
        .collect())
}

/// Compose, encode, upload, and record one unique-composition chunk.
pub async fn run_chunk(
    event: &ComposeEvent,
    source: &dyn Source,
    sink: &dyn Sink,
    opts: &ComposeOptions,
) -> Result<(ComposeOutput, ChunkStats), ComposeError> {
    let started = Instant::now();

    // ---- manifest ---------------------------------------------------------
    let manifest_started = Instant::now();
    let manifest_value = source.read_json(&event.manifest_key).await?;
    let prepared: PrepareManifest = serde_json::from_value(manifest_value)?;
    let manifest_ms = elapsed_ms(manifest_started);

    if prepared.job_id != event.job_id {
        return Err(invalid("Prepare manifest belongs to another job"));
    }
    if prepared.component_pipeline != Some(true) {
        return Err(invalid("Prepare manifest is not a component pipeline"));
    }

    let batch_index = event.batch.index;
    let compositions = select_compositions(&prepared, &event.batch)?;
    let logical_frames_emitted = compositions
        .iter()
        .map(|composition| composition.logical_frames.len())
        .sum::<usize>();
    let frame_start = compositions
        .iter()
        .flat_map(|composition| composition.logical_frames.iter().copied())
        .min()
        .ok_or_else(|| invalid("Compose batch has no logical frames"))?;
    let frame_end = compositions
        .iter()
        .flat_map(|composition| composition.logical_frames.iter().copied())
        .max()
        .ok_or_else(|| invalid("Compose batch has no logical frames"))?;

    // ---- results ----------------------------------------------------------
    let results_started = Instant::now();
    let stored_results: Vec<ComponentResult>;
    let component_results = if let Some(key) = &event.component_results_key {
        if !event.component_results.is_empty() {
            return Err(invalid("Supply either component_results or component_results_key"));
        }
        stored_results = stored_component_results(event, source.read_json(key).await?)?;
        &stored_results
    } else {
        &event.component_results
    };
    let mut results_by_task: HashMap<String, ComponentResult> = HashMap::new();
    for result in component_results {
        if results_by_task.insert(result.task_id.clone(), result.clone()).is_some() {
            return Err(invalid(format!("Duplicate component result {}", result.task_id)));
        }
    }
    let mut referenced: BTreeSet<String> = BTreeSet::new();
    for composition in &compositions {
        for layer in &composition.layers {
            referenced.insert(layer.clone());
        }
    }
    let missing: Vec<String> = referenced
        .iter()
        .filter(|task_id| !results_by_task.contains_key(*task_id))
        .cloned()
        .collect();
    if !missing.is_empty() {
        return Err(invalid(format!(
            "{} component task(s) missing results: {}",
            missing.len(),
            missing_preview(&missing)
        )));
    }
    let results_read_ms = elapsed_ms(results_started);

    // ---- canvas & coordinate space ----------------------------------------
    let settings = &prepared.settings;
    let raster_size = settings.raster_size;
    let output_size = settings.output_size;
    let (raster_canvas, output_canvas) =
        frame_canvas_sizes(&prepared.viewbox, raster_size, output_size)?;
    let declared_raster_space = prepared.component_raster_space.clone();
    let component_raster_space = declared_raster_space
        .clone()
        .unwrap_or_else(|| "raster".to_string());
    let canvas_size = match component_raster_space.as_str() {
        "output" => output_canvas,
        "raster" => raster_canvas,
        other => {
            return Err(invalid(format!(
                "Unsupported component raster space {other:?}"
            )));
        }
    };
    for result in component_results {
        if result.empty {
            continue;
        }
        let result_space = &result.component_raster_space;
        if declared_raster_space.is_some() && result_space.is_none() {
            return Err(invalid(format!(
                "Component task {} has no coordinate space",
                result.task_id
            )));
        }
        if let Some(space) = result_space {
            if space != &component_raster_space {
                return Err(invalid(format!(
                    "Component task {} uses {space:?} coordinates, expected {component_raster_space:?}",
                    result.task_id
                )));
            }
        }
    }

    // ---- download unique components --------------------------------------
    let unique_task_ids: Vec<String> = referenced
        .iter()
        .filter(|task_id| {
            let result = &results_by_task[*task_id];
            !result.empty && result.png_key.is_some()
        })
        .cloned()
        .collect();
    let workers = opts.download_concurrency.min(unique_task_ids.len().max(1));

    let download_started = Instant::now();
    let fetched = fetch_components(&unique_task_ids, &results_by_task, source, workers).await?;
    let png_download_ms = elapsed_ms(download_started);
    let png_bytes: u64 = fetched.iter().map(|(_, _, bytes)| bytes).sum();

    // ---- decode once ------------------------------------------------------
    let decode_started = Instant::now();
    let mut images: HashMap<String, RgbaImage> = HashMap::new();
    for (task_id, bytes, _) in fetched {
        let mut image = png::decode_rgba8(&bytes)?;
        // Premultiply each decoded layer once; the SIMD compositor blends
        // premultiplied RGBA and the same pixels serve every frame.
        crate::compositor::premultiply_rgba(&mut image.pixels);
        images.insert(task_id, image);
    }
    let decode_ms = elapsed_ms(decode_started);

    // ---- compose frames ---------------------------------------------------
    let mut records: Vec<FrameRecord> = Vec::with_capacity(logical_frames_emitted);
    let mut frame_canvases: HashSet<[i64; 2]> = HashSet::new();
    let mut composite_total = 0.0;
    let mut downsample_total = 0.0;
    let mut encode_total = 0.0;
    let mut upload_total = 0.0;
    let mut upload_overlap_total: f64 = 0.0;
    let mut pending_upload: Option<PendingUpload> = None;
    let downsampled_in_compose = component_raster_space == "raster" && output_size < raster_size;

    for composition in &compositions {
        let frame_number = composition.canonical_frame;
        let mut canvas = Canvas::new(canvas_size[0] as u32, canvas_size[1] as u32);

        let composite_started = Instant::now();
        for raw_task_id in &composition.layers {
            let result = results_by_task.get(raw_task_id);
            match result {
                None => continue, // unreachable after the missing-results check
                Some(result) if result.empty => continue,
                Some(result) => {
                    let layer = images
                        .get(raw_task_id)
                        .ok_or_else(|| ComposeError::missing_png(raw_task_id.clone()))?;
                    canvas.composite(layer, result.x, result.y);
                }
            }
        }
        // One unpremultiply per frame (never per layer): the canvas is
        // premultiplied during compositing and converted back to straight
        // alpha once, before the legacy downsample (FIR expects straight
        // alpha) or the PNG encoder (PNG stores straight alpha).
        crate::compositor::unpremultiply_rgba(&mut canvas.pixels);
        composite_total += elapsed_ms(composite_started);

        if downsampled_in_compose {
            let downsample_started = Instant::now();
            canvas = downsample_canvas(canvas, output_size)?;
            downsample_total += elapsed_ms(downsample_started);
        }

        let frame_canvas = [canvas.width as i64, canvas.height as i64];
        frame_canvases.insert(frame_canvas);

        // ---- encode -------------------------------------------------------
        let encode_started = Instant::now();
        let raw_png = png::encode_rgba8(canvas.width, canvas.height, &canvas.pixels)?;
        let png_path = opts.scratch_dir.join(format!("{frame_number:06}.png"));
        tokio::fs::write(&png_path, &raw_png).await?;
        if let Some(retain_dir) = &opts.retain_png_dir {
            tokio::fs::create_dir_all(retain_dir).await?;
            tokio::fs::write(retain_dir.join(format!("{frame_number:06}.png")), &raw_png).await?;
        }
        let webp_path = opts.scratch_dir.join(format!("{frame_number:06}.webp"));
        let encode_prep_ms = elapsed_ms(encode_started);
        let cwebp_started = Instant::now();
        let encode = async {
            let result = encode_webp(
                &opts.cwebp,
                settings.webp_quality,
                settings.webp_method,
                settings.webp_lossless.unwrap_or(false),
                &png_path,
                &webp_path,
            )
            .await;
            (result, elapsed_ms(cwebp_started))
        };
        let cwebp_ms = if let Some(upload) = pending_upload.take() {
            let upload_started = Instant::now();
            let upload_frame = upload.frame_number;
            let upload_key = upload.key;
            let upload_bytes = upload.bytes;
            let upload = async {
                let result = sink
                    .put_webp(upload_frame, &upload_key, &upload_bytes)
                    .await;
                (result, elapsed_ms(upload_started))
            };
            let ((encode_result, encode_ms), (upload_result, upload_ms)) =
                tokio::join!(encode, upload);
            encode_result?;
            upload_result?;
            upload_total += upload_ms;
            upload_overlap_total += encode_ms.min(upload_ms);
            encode_ms
        } else {
            let (encode_result, encode_ms) = encode.await;
            encode_result?;
            encode_ms
        };
        let encode_cleanup_started = Instant::now();
        let webp_bytes = tokio::fs::read(&webp_path).await?;
        let _ = tokio::fs::remove_file(&png_path).await;
        let _ = tokio::fs::remove_file(&webp_path).await;
        encode_total += encode_prep_ms + cwebp_ms + elapsed_ms(encode_cleanup_started);
        drop(canvas);

        // ---- queue upload -------------------------------------------------
        let webp_key = match &event.benchmark_output_prefix {
            Some(prefix) => format!("{prefix}/webp-frames/{frame_number:06}.webp"),
            None => format!(
                "jobs/{}/component/webp-frames/{frame_number:06}.webp",
                event.job_id
            ),
        };
        let sha256 = sha256_hex(&webp_bytes);
        for &logical_frame in &composition.logical_frames {
            let frame = &prepared.component_frames[logical_frame as usize - 1];
            records.push(FrameRecord {
                frame: logical_frame,
                webp_key: webp_key.clone(),
                x: 0,
                y: 0,
                width: frame_canvas[0],
                height: frame_canvas[1],
                canvas_width: frame_canvas[0],
                canvas_height: frame_canvas[1],
                duration: frame_duration(frame, &prepared.frame_durations, logical_frame),
                sha256: sha256.clone(),
                bytes: webp_bytes.len(),
            });
        }
        // Retain at most one encoded frame. Its upload is polled alongside
        // the next frame's cwebp process; the final upload is awaited below.
        pending_upload = Some(PendingUpload {
            frame_number,
            key: webp_key,
            bytes: webp_bytes,
        });
    }

    if let Some(upload) = pending_upload {
        let upload_started = Instant::now();
        sink.put_webp(upload.frame_number, &upload.key, &upload.bytes)
            .await?;
        upload_total += elapsed_ms(upload_started);
    }

    if frame_canvases.len() != 1 {
        return Err(invalid("Composed frames do not share one canvas"));
    }

    // ---- compose-batch manifest ------------------------------------------
    let batch_manifest_key = match &event.benchmark_output_prefix {
        Some(prefix) => format!("{prefix}/compose-batches/batch-{batch_index:04}.json"),
        None => format!(
            "jobs/{}/component/compose-batches/batch-{batch_index:04}.json",
            event.job_id
        ),
    };
    let manifest_write_started = Instant::now();
    let batch_manifest = BatchManifest {
        schema_version: 1,
        job_id: event.job_id.clone(),
        batch: batch_index,
        frames: records,
        warnings: Vec::new(),
    };
    let manifest_value = serde_json::to_value(&batch_manifest)?;
    sink.put_json(&batch_manifest_key, &manifest_value).await?;
    let manifest_write_ms = elapsed_ms(manifest_write_started);

    let total_ms = elapsed_ms(started);
    let unique_frames_encoded = compositions.len() as i64;
    let logical_frames_emitted = logical_frames_emitted as i64;
    let timings = Timings {
        manifest_ms,
        results_read_ms,
        png_download_ms,
        decode_ms,
        composite_ms: composite_total,
        downsample_ms: downsample_total,
        encode_ms: encode_total,
        upload_ms: upload_total,
        upload_overlap_ms: upload_overlap_total,
        manifest_write_ms,
    };
    let stats = ChunkStats {
        job_id: event.job_id.clone(),
        batch: batch_index,
        frame_start,
        frame_end,
        frames_rendered: unique_frames_encoded,
        unique_frames_encoded,
        logical_frames_emitted,
        deduplicated_frames: logical_frames_emitted - unique_frames_encoded,
        referenced_component_count: referenced.len(),
        downloaded_png_count: unique_task_ids.len(),
        png_bytes,
        canvas_width: canvas_size[0],
        canvas_height: canvas_size[1],
        raster_size,
        output_size,
        component_raster_space,
        downsampled_in_compose,
        download_concurrency: workers,
        timings,
        total_ms,
    };
    Ok((
        ComposeOutput::component_raster(event.job_id.clone(), batch_index, batch_manifest_key),
        stats,
    ))
}

/// Convenience wrapper used by both entry points: run the chunk over a
/// source/sink pair with a pre-built option set and emit the profile event.
pub async fn compose_and_report(
    event: &ComposeEvent,
    source: &dyn Source,
    sink: &dyn Sink,
    opts: &ComposeOptions,
) -> Result<ComposeOutput, ComposeError> {
    let (output, stats) = run_chunk(event, source, sink, opts).await?;
    crate::telemetry::log_profile(&stats);
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::{select_compositions, CompositionWork};
    use crate::contract::{
        BatchIndex, ComponentComposition, ComponentFrame, ManifestSettings, PrepareManifest,
    };

    #[test]
    fn validates_s3_result_manifest_identity_and_schema() {
        use serde_json::json;
        let event = serde_json::from_value(json!({
            "job_id":"job-1", "manifest_key":"prepare.json",
            "component_results_key":"results.json", "batch":{"index":0}
        })).unwrap();
        let value = json!({"schema_version":1,"job_id":"job-1",
            "prepare_manifest_key":"prepare.json",
            "component_results":[{"task_id":"a","empty":true}]});
        assert_eq!(super::stored_component_results(&event, value.clone()).unwrap().len(), 1);
        for (field, wrong) in [
            ("schema_version", json!(2)),
            ("job_id", json!("another-job")),
            ("prepare_manifest_key", json!("another-prepare.json")),
            ("component_results", json!(null)),
        ] {
            let mut invalid = value.clone();
            invalid[field] = wrong;
            assert!(super::stored_component_results(&event, invalid).is_err());
        }
    }

    fn manifest() -> PrepareManifest {
        PrepareManifest {
            job_id: "job-1".to_string(),
            frame_count: 3,
            viewbox: vec![0.0, 0.0, 100.0, 100.0],
            frame_durations: vec![40, 50, 60],
            settings: ManifestSettings {
                raster_size: 256,
                output_size: 256,
                webp_quality: 85.0,
                webp_method: 4,
                webp_lossless: Some(false),
            },
            component_pipeline: Some(true),
            component_raster_space: Some("output".to_string()),
            component_frames: vec![
                ComponentFrame {
                    number: 1,
                    layers: vec!["a".to_string()],
                    duration_ms: Some(40),
                },
                ComponentFrame {
                    number: 2,
                    layers: vec!["b".to_string()],
                    duration_ms: Some(50),
                },
                ComponentFrame {
                    number: 3,
                    layers: vec!["a".to_string()],
                    duration_ms: Some(60),
                },
            ],
            component_compositions: vec![
                ComponentComposition {
                    canonical_frame: 1,
                    layers: vec!["a".to_string()],
                    logical_frames: vec![1, 3],
                },
                ComponentComposition {
                    canonical_frame: 2,
                    layers: vec!["b".to_string()],
                    logical_frames: vec![2],
                },
            ],
        }
    }

    #[test]
    fn selects_one_global_composition_with_noncontiguous_aliases() {
        let selected = select_compositions(
            &manifest(),
            &BatchIndex {
                index: 0,
                frame_start: None,
                frame_end: None,
                composition_start: Some(0),
                composition_end: Some(0),
            },
        )
        .unwrap();
        assert_eq!(
            selected,
            vec![CompositionWork {
                canonical_frame: 1,
                layers: vec!["a".to_string()],
                logical_frames: vec![1, 3],
            }]
        );
    }

    #[test]
    fn preserves_legacy_contiguous_frame_batches() {
        let selected = select_compositions(
            &manifest(),
            &BatchIndex {
                index: 0,
                frame_start: Some(2),
                frame_end: Some(3),
                composition_start: None,
                composition_end: None,
            },
        )
        .unwrap();
        assert_eq!(selected.len(), 2);
        assert_eq!(selected[0].logical_frames, vec![2]);
        assert_eq!(selected[1].logical_frames, vec![3]);
    }

    #[test]
    fn rejects_a_recipe_partition_with_duplicate_logical_frames() {
        let mut prepared = manifest();
        prepared.component_compositions[1].logical_frames.push(3);
        assert!(select_compositions(
            &prepared,
            &BatchIndex {
                index: 0,
                frame_start: None,
                frame_end: None,
                composition_start: Some(0),
                composition_end: Some(0),
            },
        )
        .is_err());
    }
}
