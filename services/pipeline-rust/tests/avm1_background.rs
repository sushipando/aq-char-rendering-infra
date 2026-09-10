//! Opt-in local regressions for cached AVM1 backgrounds. No AWS clients.
use anyhow::{ensure, Result};
use aqw_render_pipeline::{
    self as pipeline,
    export::{export_source, ExportOptions},
    model::SourceManifest,
    store::{self, FsStore, Store},
    timeline::Selection,
};
use serde_json::json;
use std::{collections::BTreeSet, time::Duration};

#[tokio::test]
#[ignore = "requires AQW_TEST_SHADOWFALL_WRAPPED_SWF and AQW_TEST_FFDEC; local export/raster, no AWS"]
async fn shadowfall_avm1_stops_export_and_warm_cache() -> Result<()> {
    background_export(
        "shadowfall",
        "AQW_TEST_SHADOWFALL_WRAPPED_SWF",
        "782caf7d4431c6534960eae224bb68056cb7c1c1372935e4163fb5bfdfe8e93e",
        &[145, 235],
        0,
        5,
    )
    .await
}

#[tokio::test]
#[ignore = "requires AQW_TEST_RIVER_WRAPPED_SWF and AQW_TEST_FFDEC; local export/raster, no AWS"]
async fn river_construction_metadata_preserves_static_artwork_and_cache() -> Result<()> {
    background_export(
        "river",
        "AQW_TEST_RIVER_WRAPPED_SWF",
        "1e8f035fe5714d313cd04e162b0e1c99cdaf9750bc6eecc0e7e136998c8e4228",
        &[],
        2,
        1,
    )
    .await
}

async fn background_export(
    name: &str,
    fixture: &str,
    hash: &str,
    stops: &[u16],
    metadata_placements: usize,
    expected_raster_states: usize,
) -> Result<()> {
    let bytes = std::fs::read(std::env::var(fixture)?)?;
    ensure!(pipeline::sha256(&bytes) == hash, "wrong {name} fixture");
    let root = tempfile::Builder::new()
        .prefix("aqw-avm1-visual-")
        .tempdir()?
        .keep();
    println!("{name} regression artifacts: {}", root.display());
    let store = FsStore(root.join("objects"));
    let key = "presentation_background";
    let source_key = format!("{name}.swf");
    let job = format!("{name}-regression");
    let source = json!({"idx":0,"key":source_key,"sha256":pipeline::sha256(&bytes),"requests":[{
        "key":key,"character_id":65533,"class_name":"CharpageBackgroundStage","frame":1,
        "root_timeline_frames":1,"ancestor_names":[key,"mcChar","stage"]}]});
    store
        .put(
            "source",
            &source_key,
            bytes,
            "application/octet-stream",
            true,
        )
        .await?;
    store::write(
        &store,
        "work",
        "input.json",
        &json!({"job_id":job,"sources":[source.clone()],
        "settings":{"zoom":1.0,"subframe_start":1},"export_frame_count":120}),
        false,
    )
    .await?;
    let event = json!({"job_id":job,"input_key":"input.json","source":source});
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
    ensure!(
        manifest.symbols[key].schedule.len() == 120,
        "lost output frames"
    );
    for id in stops {
        ensure!(
            manifest
                .timeline_decisions
                .iter()
                .any(|d| d.character_id == *id && d.selection == Selection::Hold { frame: 1 }),
            "authored stop on sprite {id} was not preserved"
        );
    }
    ensure!(
        manifest
            .timeline_warnings
            .iter()
            .filter(|s| s.contains("nonvisual construction metadata"))
            .count()
            == metadata_placements,
        "construction metadata diagnostics were lost"
    );
    let mut pixels = BTreeSet::new();
    for (n, state) in manifest.states.values().enumerate() {
        let svg = store.get("work", &state.svg_key).await?.unwrap();
        let tree = resvg::usvg::Tree::from_data(&svg, &resvg::usvg::Options::default())?;
        let mut pixmap = resvg::tiny_skia::Pixmap::new(550, 350).unwrap();
        let scale = (550.0 / tree.size().width()).min(350.0 / tree.size().height());
        resvg::render(
            &tree,
            resvg::tiny_skia::Transform::from_scale(scale, scale),
            &mut pixmap.as_mut(),
        );
        ensure!(
            pixmap.data().chunks_exact(4).any(|p| p[3] > 0),
            "empty background state"
        );
        pixels.insert(pipeline::sha256(pixmap.data()));
        if n < 3 {
            pixmap.save_png(root.join(format!("{name}-{n}.png")))?;
        }
    }
    ensure!(
        pixels.len() == expected_raster_states,
        "unexpected background raster states"
    );
    ensure!(
        export_source(&store, "work", "source", &event, options()).await?["vector_cache_hit"]
            == true,
        "warm cache missed"
    );
    println!("{name}: 120 exported frames; {} SVG states; {} raster states; authored controls and warm cache verified",manifest.states.len(),pixels.len());
    Ok(())
}
