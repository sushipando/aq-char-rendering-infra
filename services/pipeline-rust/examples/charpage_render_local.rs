//! One-frame visual validation using the real prepare/raster path and local files.
//! Downloads missing official SWFs only when --allow-official-downloads is passed.
//! Never connects to AWS. All artifacts stay in the supplied output directory.
use anyhow::{ensure, Context, Result};
use aqw_render_pipeline::{
    self as p,
    model::*,
    store::{self, Store},
};
use serde_json::{json, Value};
use std::{collections::BTreeMap, path::PathBuf, time::Duration};
#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    ensure!(args.len() >= 5, "usage: charpage_render_local FIELDS.json CHARACTER_B.swf FFDEC.jar OUTPUT_DIR [--allow-official-downloads] [--facing-left]");
    let fields: Value = serde_json::from_slice(&std::fs::read(&args[1])?)?;
    let character = std::fs::read(&args[2])?;
    let root = PathBuf::from(&args[4]);
    std::fs::create_dir_all(&root)?;
    let store = store::FsStore(root.clone());
    let config = p::config::Config {
        source_bucket: "source".into(),
        work_bucket: "work".into(),
        job_table: "unused".into(),
        result_queue_url: "unused".into(),
        bounds_queue_url: None,
        public_base_url: "https://example.test".into(),
        dataset_version: "local".into(),
        asset_manifest_key: "dataset.json".into(),
        renderer_version: "local-charpage".into(),
        ffdec: args[3].clone().into(),
        render_cache: false,
        official_fallback: args.iter().any(|a| a == "--allow-official-downloads"),
        component_frame_cap: 1,
        compose_concurrency: 1,
        frames_per_lambda: 1,
        download_concurrency: 4,
        bounds_resolution: 256,
        bounds_padding_pixels: 1,
        worker_concurrency: BTreeMap::new(),
        defaults: json!({}),
    };
    let reference =
        json!({"key":"characterB.swf","sha256":p::sha256(&character),"size":character.len()});
    store
        .put(
            "source",
            "characterB.swf",
            character,
            "application/octet-stream",
            false,
        )
        .await?;
    store::write(&store,"source","dataset.json",&json!({"schema_version":1,"dataset_version":"local","assets":{},"character_renderer":reference}),false).await?;
    let job = "6ccae399-708b-4f35-b830-6a0d756f21c1";
    let request = p::contract::request(
        json!({"schema_version":1,"job_id":job,"created_at":"2026-09-07T00:00:00Z","discord":{"user_id":"1","channel_id":"2"},"appearance":fields,"render":{"username":fields["strName"],"view":"charpage","complete_loop":false,"max_frames":1,"raster_size":1024,"output_size":1024,"zoom":1.0,"facing":if args.iter().any(|a|a=="--facing-left"){"left"}else{"right"}}}),
        None,
    )?;
    let resolved = p::resolve::resolve(&store, &config, &request).await?;
    let mut sources = BTreeMap::new();
    for source in resolved["sources"].as_array().context("sources")? {
        let result = p::export::export_source(
            &store,
            "work",
            "source",
            &json!({"job_id":job,"input_key":resolved["input_key"],"source":source}),
            p::export::ExportOptions {
                jar: config.ffdec.clone(),
                timeout: Duration::from_secs(280),
                bounds_prefetch: None,
            },
        )
        .await?;
        sources.insert(
            source["idx"].as_u64().context("source idx")? as usize,
            result["manifest_key"]
                .as_str()
                .context("export manifest")?
                .to_owned(),
        );
    }
    let planned = p::bounds::plan(
        &store,
        "work",
        job,
        resolved["input_key"].as_str().unwrap(),
        sources,
        ProbeConfig::new(1.0),
        p::bounds::PlanOptions::new(true, p::bounds::BoundsMode::Inline),
    )
    .await?;
    let tasks: Vec<ProbeTask> =
        store::read(&store, "work", planned["tasks_key"].as_str().unwrap()).await?;
    for task in tasks {
        p::bounds::run_probe(&store, "work", &task).await?;
    }
    let finished = p::finish::finish(&store,&config,&json!({"request":request,"input_key":resolved["input_key"],"bounds_plan_key":planned["plan_key"]})).await?;
    let manifest: Value =
        store::read(&store, "work", finished["manifest_key"].as_str().unwrap()).await?;
    let raster_store = aqw_component_raster::storage::FsStore::new(root.clone());
    let mut images = BTreeMap::new();
    for index in 0..manifest["component_tasks"].as_array().unwrap().len() {
        let result = aqw_component_raster::worker::run_raster_task(
            &serde_json::from_value(
                json!({"job_id":job,"manifest_key":finished["manifest_key"],"task_index":index}),
            )?,
            &raster_store,
            &raster_store,
        )
        .await?;
        if !result.empty {
            let bytes = store
                .get("work", result.png_key.as_ref().context("png key")?)
                .await?
                .context("png")?;
            let mut image = aqw_component_compose::png::decode_rgba8(&bytes)?;
            aqw_component_compose::compositor::premultiply_rgba(&mut image.pixels);
            images.insert(result.task_id, (image, result.x, result.y));
        }
    }
    let background = store
        .get(
            "work",
            manifest["presentation_layers"]["background"]["key"]
                .as_str()
                .unwrap(),
        )
        .await?
        .unwrap();
    let mut image = aqw_component_compose::png::decode_rgba8(&background)?;
    aqw_component_compose::compositor::premultiply_rgba(&mut image.pixels);
    let mut canvas = aqw_component_compose::compositor::Canvas::from_pixels(
        image.width,
        image.height,
        image.pixels,
    );
    for id in manifest["component_frames"][0]["layers"]
        .as_array()
        .unwrap()
    {
        if let Some((image, x, y)) = images.get(id.as_str().unwrap()) {
            canvas.composite(image, *x, *y);
        }
    }
    let foreground = store
        .get(
            "work",
            manifest["presentation_layers"]["foreground"]["key"]
                .as_str()
                .unwrap(),
        )
        .await?
        .unwrap();
    let mut image = aqw_component_compose::png::decode_rgba8(&foreground)?;
    aqw_component_compose::compositor::premultiply_rgba(&mut image.pixels);
    canvas.composite(&image, 0, 0);
    aqw_component_compose::compositor::unpremultiply_rgba(&mut canvas.pixels);
    std::fs::write(
        root.join("charpage.png"),
        aqw_component_compose::png::encode_rgba8(canvas.width, canvas.height, &canvas.pixels)?,
    )?;
    Ok(())
}
