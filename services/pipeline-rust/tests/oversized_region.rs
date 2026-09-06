//! Opt-in replay of a saved FFDec SVG using its frozen placement/appearance.
use anyhow::{Context, Result};
use aqw_render_pipeline::{
    bounds,
    model::*,
    store::{self, FsStore, Store},
};
use serde_json::{json, Value};

#[tokio::test]
#[ignore = "requires AQW_TEST_REGION_SVG and AQW_TEST_REGION_MANIFEST; saved oversized pet, no AWS"]
async fn oversized_pet_region_preserves_worker_png_and_placement() -> Result<()> {
    let bytes = std::fs::read(std::env::var("AQW_TEST_REGION_SVG")?)?;
    let mut manifest: Value =
        serde_json::from_slice(&std::fs::read(std::env::var("AQW_TEST_REGION_MANIFEST")?)?)?;
    let state = StateRef::new(&bytes);
    let mut task = manifest["component_tasks"]
        .as_array()
        .context("tasks")?
        .iter()
        .find(|t| t["symbol_key"] == "pet" && t["state_signature"].as_str() == Some(&state.sha256))
        .context("matching pet task")?
        .clone();
    let probe = bounds::probe(
        &bytes,
        &ProbeTask::new(
            state.clone(),
            ProbeConfig::new(manifest["settings"]["zoom"].as_f64().context("zoom")?),
        )?,
    )?;
    task["svg_key"] = state.svg_key.clone().into();
    task["state_signature"] = state.sha256.clone().into();
    task.as_object_mut().unwrap().remove("raster_bounds");
    manifest["job_id"] = "region-test".into();
    manifest["cache"] = json!({"components":false});
    manifest["component_tasks"] = json!([task]);
    let temp = tempfile::tempdir()?;
    let store = FsStore(temp.path().into());
    store
        .put("work", &state.svg_key, bytes, "image/svg+xml", true)
        .await?;
    let raster_store = aqw_component_raster::storage::FsStore::new(temp.path().into());
    let mut results = Vec::new();
    let mut pngs = Vec::new();
    for bounded in [false, true] {
        if bounded {
            manifest["component_tasks"][0]["raster_bounds"] =
                json!({"policy":aqw_component_raster::region::POLICY,"bounds":probe.bounds});
        }
        store::write(&store, "work", "manifest.json", &manifest, false).await?;
        let result = aqw_component_raster::worker::run_raster_task(
            &serde_json::from_value(
                json!({"job_id":"region-test","manifest_key":"manifest.json","task_index":0}),
            )?,
            &raster_store,
            &raster_store,
        )
        .await?;
        pngs.push(
            store
                .get("work", result.png_key.as_deref().context("png")?)
                .await?
                .context("png missing")?,
        );
        results.push(result);
    }
    assert_eq!(
        (
            results[0].x,
            results[0].y,
            results[0].width,
            results[0].height
        ),
        (
            results[1].x,
            results[1].y,
            results[1].width,
            results[1].height
        )
    );
    assert_eq!(pngs[0], pngs[1], "final component PNG bytes changed");
    eprintln!(
        "pet parity: probe={:?}, raster_ms={:?}",
        probe.bounds,
        results.iter().map(|r| r.rasterize_ms).collect::<Vec<_>>()
    );
    Ok(())
}
