//! Real cached aura assets through export, import, tinting and CPU rasterization.
use anyhow::{ensure, Result};
use aqw_component_raster::{component_svg, import, raster, storage, svg};
use aqw_render_pipeline::{
    self as pipeline,
    export::{export_source, ExportOptions},
    model::SourceManifest,
    store::{self, FsStore, Store},
};
use serde_json::{json, Value};
use std::{
    collections::{BTreeSet, HashMap},
    path::PathBuf,
    time::Duration,
};

#[test]
fn separate_additive_passes_match_drawing_against_the_real_backdrop() -> Result<()> {
    use aqw_component_compose::compositor::Canvas;
    let body = r##"<g transform="translate(1 1)"><rect width="5" height="5" fill="#004000"/><g style="mix-blend-mode: aqw-add"><rect width="6" height="6" fill="#800000"/></g><rect width="1" height="1" fill="#ffffff"/></g>"##;
    let svg = |body: &str| {
        format!(r#"<svg xmlns="http://www.w3.org/2000/svg" width="8" height="8">{body}</svg>"#)
    };
    let backdrop = r##"<rect width="8" height="8" fill="#204080"/>"##;
    let tree = resvg::usvg::Tree::from_str(&svg(body), &Default::default())?;
    let plans = resvg::layers::plan(&tree);
    ensure!(plans.iter().map(|p| p.additive).collect::<Vec<_>>() == [false, true, false]);
    let mut canvas = Canvas::from_pixels(8, 8, [32, 64, 128, 255].repeat(64));
    for plan in plans {
        let mut image = resvg::tiny_skia::Pixmap::new(8, 8).unwrap();
        resvg::layers::render(
            &plan,
            resvg::tiny_skia::Transform::identity(),
            &mut image.as_mut(),
        );
        let image = aqw_component_compose::compositor::RgbaImage::new(8, 8, image.take());
        if plan.additive {
            canvas.composite_additive(&image, 0, 0);
        } else {
            canvas.composite(&image, 0, 0);
        }
    }
    let tree =
        resvg::usvg::Tree::from_str(&svg(&format!("{backdrop}{body}")), &Default::default())?;
    let mut direct = resvg::tiny_skia::Pixmap::new(8, 8).unwrap();
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::identity(),
        &mut direct.as_mut(),
    );
    ensure!(canvas.pixels == direct.data());
    // A filter establishes an isolated scope; its internal Add must stay inside it.
    let scoped = svg(&format!(
        r##"<filter id="f"><feColorMatrix/></filter><g filter="url(#f)">{body}</g>"##
    ));
    let tree = resvg::usvg::Tree::from_str(&scoped, &Default::default())?;
    ensure!(resvg::layers::plan(&tree).iter().all(|p| !p.additive));
    Ok(())
}

#[tokio::test]
#[ignore = "requires AQW_TEST_AURA_DIR with first-frame SVGs and AQW_TEST_AURA_EXPORTS from the export regression; no AWS"]
async fn saved_taizou_frame_keeps_additive_layers_through_workers() -> Result<()> {
    use aqw_component_compose::{compositor, png};
    let fixture = PathBuf::from(std::env::var("AQW_TEST_AURA_DIR")?);
    let exports = PathBuf::from(std::env::var("AQW_TEST_AURA_EXPORTS")?);
    let mut manifest: Value =
        serde_json::from_slice(&std::fs::read(fixture.join("manifest.json"))?)?;
    let first = manifest["component_compositions"][0].clone();
    let ids: BTreeSet<_> = first["layers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_owned())
        .collect();
    manifest["component_tasks"]
        .as_array_mut()
        .unwrap()
        .retain(|t| ids.contains(t["task_id"].as_str().unwrap()));
    manifest["cache"]["components"] = json!(false);
    manifest["settings"]["raster_size"] = json!(900);
    manifest["settings"]["output_size"] = json!(900);
    manifest["component_raster_space"] = json!("output");
    manifest["presentation_layers"] = Value::Null;
    let store = FsStore(fixture.join("objects"));
    for entry in std::fs::read_dir(exports.join("objects/work/vector-manifests/6"))? {
        let m: SourceManifest = serde_json::from_slice(&std::fs::read(entry?.path())?)?;
        for (key, symbol) in &m.symbols {
            let state = &m.states[&symbol.schedule[0]];
            let data = std::fs::read(exports.join("objects/work").join(&state.svg_key))?;
            store
                .put("work", &state.svg_key, data, "image/svg+xml", true)
                .await?;
            manifest["parts"][key]["character_id"] = json!(symbol.request.character_id);
            manifest["parts"][key]["root_class"] = json!(symbol.request.class_name);
            manifest["parts"][key]["placement_colors"] = serde_json::to_value(&m.placement_colors)?;
            manifest["parts"][key]["color_rules"] = serde_json::to_value(&m.color_rules)?;
            manifest["parts"][key]["hand_visibility"] = serde_json::to_value(&m.hand_visibility)?;
            for task in manifest["component_tasks"].as_array_mut().unwrap() {
                if task["symbol_key"] == *key {
                    task["svg_key"] = json!(state.svg_key);
                    task["state_signature"] = json!(state.sha256);
                    task.as_object_mut().unwrap().remove("raster_bounds");
                }
            }
        }
    }
    store::write(&store, "work", "preview.json", &manifest, false).await?;
    let raster_store = storage::FsStore::new(fixture.join("objects"));
    let mut results = HashMap::new();
    for index in 0..manifest["component_tasks"].as_array().unwrap().len() {
        let result = aqw_component_raster::worker::run_raster_task(&serde_json::from_value(json!({"job_id":manifest["job_id"],"manifest_key":"preview.json","task_index":index}))?,&raster_store,&raster_store).await?;
        println!(
            "{}: {} compositing passes",
            result.symbol_key,
            result.layers.len()
        );
        results.insert(result.task_id.clone(), result);
    }
    let handoff = pipeline::components::collect(
        &store,
        "work",
        &json!({"job_id":manifest["job_id"],"manifest_key":"preview.json"}),
    )
    .await?;
    let compact: Value =
        store::read(&store, "work", handoff["manifest_key"].as_str().unwrap()).await?;
    ensure!(
        compact.to_string().contains("\"blend_mode\":\"add\""),
        "collection lost additive layers"
    );
    let mut canvas = compositor::Canvas::new(900, 573);
    let mut flattened = compositor::Canvas::new(900, 573);
    for id in first["layers"].as_array().unwrap() {
        let r = &results[id.as_str().unwrap()];
        if r.empty {
            continue;
        }
        let mut layers: Vec<_> = r
            .layers
            .iter()
            .map(|l| (&l.png_key, l.x, l.y, l.blend_mode == "add"))
            .collect();
        if let Some(key) = &r.png_key {
            layers.push((key, r.x, r.y, false));
        }
        for (key, x, y, add) in layers {
            let mut image = png::decode_rgba8(&store.get("work", key).await?.unwrap())?;
            compositor::premultiply_rgba(&mut image.pixels);
            if add {
                canvas.composite_additive(&image, x, y);
            } else {
                canvas.composite(&image, x, y);
            }
            flattened.composite(&image, x, y);
        }
    }
    ensure!(canvas.pixels != flattened.pixels);
    for (name, mut image) in [
        ("taizou-composed", canvas),
        ("taizou-without-add", flattened),
    ] {
        compositor::unpremultiply_rgba(&mut image.pixels);
        std::fs::write(
            fixture.join(format!("{name}.png")),
            png::encode_rgba8(image.width, image.height, &image.pixels)?,
        )?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires AQW_TEST_AURA_DIR (cached input.json, gauntlet.swf, armor.swf), patched AQW_TEST_FFDEC; no AWS"]
async fn real_taizou_additive_auras_and_one_axis_blurs() -> Result<()> {
    let fixture = PathBuf::from(std::env::var("AQW_TEST_AURA_DIR")?);
    let input: Value = serde_json::from_slice(&std::fs::read(fixture.join("input.json"))?)?;
    let root = tempfile::Builder::new()
        .prefix("aqw-aura-visual-")
        .tempdir()?
        .keep();
    println!("Aura regression artifacts: {}", root.display());
    let store = FsStore(root.join("objects"));
    for (index, file, expected_hash) in [
        (
            0,
            "gauntlet.swf",
            "3f4f603475f2594a26a2e2bd5463dc6cc37f5286eb6275d5af753c94b207414b",
        ),
        (
            9,
            "armor.swf",
            "7768273fef5d447a2b06261945ee772d4f2fcc7b51428756a42dd75b2f11b13a",
        ),
        (
            16,
            "armor.swf",
            "7768273fef5d447a2b06261945ee772d4f2fcc7b51428756a42dd75b2f11b13a",
        ),
    ] {
        let source = input["sources"][index].clone();
        let bytes = std::fs::read(fixture.join(file))?;
        ensure!(pipeline::sha256(&bytes) == expected_hash);
        ensure!(pipeline::sha256(&bytes) == source["sha256"].as_str().unwrap());
        store
            .put(
                "source",
                source["key"].as_str().unwrap(),
                bytes,
                "application/octet-stream",
                true,
            )
            .await?;
        let mut prepared = input.clone();
        prepared["export_frame_count"] = json!(12);
        store::write(&store, "work", "input.json", &prepared, false).await?;
        let event = json!({"job_id":input["job_id"],"input_key":"input.json","source":source});
        let options = || {
            ExportOptions::without_prefetch(
                std::env::var("AQW_TEST_FFDEC").unwrap().into(),
                Duration::from_secs(240),
            )
        };
        let result = export_source(&store, "work", "source", &event, options()).await?;
        let manifest: SourceManifest =
            store::read(&store, "work", result["manifest_key"].as_str().unwrap()).await?;
        manifest.validate()?;
        let (key, symbol) = manifest.symbols.iter().next().unwrap();
        ensure!(symbol.schedule.len() == 12);
        let colors = manifest
            .color_rules
            .iter()
            .map(|(k, v)| (k.clone(), (v[0].clone(), v[1].clone())))
            .collect::<HashMap<_, _>>();
        let fields = input["fields"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_owned()))
            .collect::<HashMap<_, _>>();
        let placement = storage::parse_placement_key_string(&serde_json::from_value(
            serde_json::to_value(&manifest.placement_colors)?,
        )?);
        let placement = placement.into_iter().map(|(k, v)| (k, v.into())).collect();
        let hand = manifest
            .hand_visibility
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let mut rasters = BTreeSet::new();
        for (frame, hash) in symbol.schedule.iter().enumerate() {
            let data = store
                .get("work", &manifest.states[hash].svg_key)
                .await?
                .unwrap();
            let source = std::str::from_utf8(&data)?;
            ensure!(!source.contains("NaN"), "invalid blur escaped the exporter");
            ensure!(
                source.contains("aqw-add"),
                "authored Add blend lost; use patched FFDec"
            );
            let imported = import::import_ffdec_symbol_with_visibility(
                key,
                source,
                1.0,
                &colors,
                &symbol.request.class_name,
                &placement,
                Some(symbol.request.character_id.into()),
                &hand,
                Some("fronthand"),
            )?;
            let built = component_svg::build_component_svg(
                &imported,
                import::IDENTITY,
                false,
                key,
                key,
                imported.bounds,
                600,
                &fields,
                &colors.values().cloned().collect::<Vec<_>>(),
            );
            let encoded = svg::serialize(&svg::Document {
                root: built.root,
                namespaces: built.namespaces,
            });
            let size = (
                (built.page[2] - built.page[0]) as u32,
                (built.page[3] - built.page[1]) as u32,
            );
            let fixed = raster::render_svg_resvg(encoded.as_bytes(), size)?;
            rasters.insert(pipeline::sha256(&fixed.pixels));
            if frame == 0 {
                let missing = raster::render_svg_resvg(
                    encoded.replace("aqw-add", "normal").as_bytes(),
                    size,
                )?;
                let energy = |p: &[u8]| {
                    p.chunks_exact(4)
                        .map(|c| (c[0] as u64 + c[1] as u64 + c[2] as u64) * c[3] as u64)
                        .sum::<u64>()
                };
                ensure!(
                    energy(&fixed.pixels) > energy(&missing.pixels),
                    "Add blending did not restore emission"
                );
                for (label, image) in [("fixed", fixed), ("without-add", missing)] {
                    std::fs::write(
                        root.join(format!("{key}-{label}.png")),
                        raster::encode_rgba8(image.width, image.height, &image.pixels)?,
                    )?;
                }
                std::fs::write(root.join(format!("{key}-fixed.svg")), encoded)?;
            }
        }
        if file == "armor.swf" {
            ensure!(rasters.len() > 1, "armor effects froze");
        }
        ensure!(
            export_source(&store, "work", "source", &event, options()).await?["vector_cache_hit"]
                == true
        );
        println!("{key}: 12 frames, {} distinct rasters; finite blurs, brighter additive effects, warm cache verified", rasters.len());
    }
    Ok(())
}
