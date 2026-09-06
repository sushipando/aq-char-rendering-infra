use anyhow::{Context, Result};
use aqw_render_pipeline::{
    self as pipeline, bounds,
    config::Config,
    finish,
    model::*,
    store::{self, FsStore, Store},
};
use serde_json::{json, Value};
use std::{collections::BTreeMap, path::PathBuf, time::Duration};

fn config() -> Config {
    Config {
        source_bucket: "source".into(),
        work_bucket: "work".into(),
        job_table: "jobs".into(),
        result_queue_url: "https://example.test/results".into(),
        bounds_queue_url: None,
        public_base_url: "https://example.test".into(),
        dataset_version: "dev-v1".into(),
        asset_manifest_key: "datasets/dev-v1/manifest.json".into(),
        renderer_version: "v20-rust-bounds".into(),
        ffdec: std::env::var("AQW_TEST_FFDEC")
            .unwrap_or_else(|_| "/opt/ffdec/ffdec-cli.jar".into())
            .into(),
        render_cache: false,
        official_fallback: false,
        component_frame_cap: 120,
        compose_concurrency: 4,
        frames_per_lambda: 1,
        download_concurrency: 4,
        bounds_resolution: 256,
        bounds_padding_pixels: 1,
        worker_concurrency: BTreeMap::new(),
        defaults: json!({}),
    }
}
const JOB: &str = "45cfafbd-5089-4f6d-850a-caa798ec1fcb";

struct ComposeStore<'a>(&'a FsStore);
fn compose_error(error: anyhow::Error) -> aqw_component_compose::error::ComposeError {
    aqw_component_compose::error::ComposeError::invalid(error.to_string())
}
#[async_trait::async_trait]
impl aqw_component_compose::storage::Source for ComposeStore<'_> {
    async fn read_json(
        &self,
        key: &str,
    ) -> Result<Value, aqw_component_compose::error::ComposeError> {
        store::read(self.0, "work", key)
            .await
            .map_err(compose_error)
    }
    async fn fetch_bytes(
        &self,
        key: &str,
        expected: Option<&str>,
    ) -> Result<Vec<u8>, aqw_component_compose::error::ComposeError> {
        let bytes = self
            .0
            .get("work", key)
            .await
            .map_err(compose_error)?
            .context("missing object")
            .map_err(compose_error)?;
        if expected.is_some_and(|hash| hash != pipeline::sha256(&bytes)) {
            return Err(compose_error(anyhow::anyhow!("checksum mismatch")));
        }
        Ok(bytes)
    }
}
#[async_trait::async_trait]
impl aqw_component_compose::storage::Sink for ComposeStore<'_> {
    async fn put_webp(
        &self,
        _: i64,
        key: &str,
        bytes: &[u8],
    ) -> Result<(), aqw_component_compose::error::ComposeError> {
        self.0
            .put("work", key, bytes.to_vec(), "image/webp", false)
            .await
            .map_err(compose_error)
    }
    async fn put_json(
        &self,
        key: &str,
        value: &Value,
    ) -> Result<(), aqw_component_compose::error::ComposeError> {
        store::write(self.0, "work", key, value, false)
            .await
            .map_err(compose_error)
    }
}

async fn synthetic(store: &dyn Store) -> Result<(Value, Value)> {
    let request = pipeline::contract::request(
        json!({"schema_version":1,"job_id":JOB,"created_at":"2026-09-05T00:00:00Z","discord":{"user_id":"1","channel_id":"2"},"render":{"username":"Test","max_frames":8,"raster_size":256,"output_size":128,"zoom":1.0}}),
        None,
    )?;
    let mut states = BTreeMap::new();
    let mut schedule = Vec::new();
    for color in ["red", "blue"] {
        let bytes=format!(r#"<svg xmlns="http://www.w3.org/2000/svg" width="100" height="100"><g transform="matrix(1 0 0 1 30 40)"><rect x="-10" y="-10" width="30" height="20" fill="{color}"/></g></svg>"#).into_bytes();
        let state = StateRef::new(&bytes);
        store
            .put("work", &state.svg_key, bytes, "image/svg+xml", true)
            .await?;
        schedule.push(state.sha256.clone());
        states.insert(state.sha256.clone(), state);
    }
    let mut symbols = BTreeMap::new();
    for key in ["cape", "ground"] {
        symbols.insert(
            key.into(),
            SymbolExport {
                request: SymbolRequest {
                    key: key.into(),
                    class_name: "Test".into(),
                    character_id: 1,
                    frame: 1,
                    root_timeline_frames: 1,
                },
                schedule: (0..16).map(|i| schedule[i % 2].clone()).collect(),
                mirror_flip_frame: 0,
                random_pose_as3: false,
                animated_span: 0,
                settled_stop_frame: None,
            },
        );
    }
    let manifest = SourceManifest {
        schema_version: VECTOR_SCHEMA,
        export_policy: EXPORT_POLICY.into(),
        source_sha256: "a".repeat(64),
        export_identity: "fixture".into(),
        symbols,
        states,
        color_rules: BTreeMap::new(),
        placement_colors: BTreeMap::new(),
        timeline_decisions: Vec::new(),
    };
    store::write(
        store,
        "work",
        "vector-manifests/fixture.json",
        &manifest,
        true,
    )
    .await?;
    let prepared = json!({"schema_version":1,"job_id":JOB,"settings":request["render"],"sources":[{"idx":0,"sha256":"a".repeat(64)}],"render_hash":"fixture","final_key":"renders/fixture.webp","fields":{},"aliases":{"cape":"cape","ground":"ground"},"weapon_type":"Sword","warnings":[],"frame_rate":24.0,"export_frame_count":16});
    store::write(store, "work", "jobs/input.json", &prepared, false).await?;
    let result = bounds::plan(
        store,
        "work",
        JOB,
        "jobs/input.json",
        BTreeMap::from([(0, "vector-manifests/fixture.json".into())]),
        ProbeConfig::new(1.0),
        bounds::PlanOptions::new(true, bounds::BoundsMode::Inline),
    )
    .await?;
    Ok((request, result))
}

#[tokio::test]
async fn global_dedup_barrier_cache_and_direct_component_contract() -> Result<()> {
    let temporary = tempfile::tempdir()?;
    let store = FsStore(temporary.path().into());
    let (request, planned) = synthetic(&store).await?;
    assert_eq!(planned["task_count"], 2); // Two states across two symbols / 32 exported frames.
    assert_eq!(planned["bounds_mode"], "inline");
    assert_eq!(planned["inline_tasks"].as_array().unwrap().len(), 2);
    let event = json!({"request":request,"input_key":"jobs/input.json","bounds_plan_key":planned["plan_key"]});
    assert!(finish::finish(&store, &config(), &event).await.is_err());
    let tasks: Vec<ProbeTask> =
        store::read(&store, "work", planned["tasks_key"].as_str().unwrap()).await?;
    for task in &tasks {
        bounds::run_probe(&store, "work", task).await?;
    }
    let prepared = finish::finish(&store, &config(), &event).await?;
    assert_eq!(prepared["frame_count"], 8);
    assert_eq!(prepared["unique_composition_count"], 2);
    assert_eq!(
        prepared["component_batches"],
        json!([
            {"index":0,"composition_start":0,"composition_end":0},
            {"index":1,"composition_start":1,"composition_end":1}
        ])
    );
    assert_eq!(
        prepared["component_task_indices"].as_array().unwrap().len(),
        4
    ); // two placements x two states
    let manifest: Value =
        store::read(&store, "work", prepared["manifest_key"].as_str().unwrap()).await?;
    assert_eq!(manifest["component_compose_schema"], 2);
    assert_eq!(
        manifest["component_compositions"].as_array().unwrap().len(),
        2
    );
    assert_eq!(
        manifest["component_compositions"][0]["logical_frames"],
        json!([1, 3, 5, 7])
    );
    assert_eq!(
        manifest["component_compositions"][1]["logical_frames"],
        json!([2, 4, 6, 8])
    );
    assert!(manifest["component_tasks"]
        .as_array()
        .unwrap()
        .iter()
        .all(|task| task.get("svg_key").is_some() && task.get("bundle_key").is_none()));
    for task in manifest["component_tasks"].as_array().unwrap() {
        let hint: aqw_component_raster::contract::RasterBounds = serde_json::from_value(task["raster_bounds"].clone())?;
        hint.validate()?;
        let probe_task = tasks.iter().find(|p| p.state.sha256 == task["state_signature"].as_str().unwrap()).unwrap();
        let measured: BoundsResult = store::read(&store, "work", &probe_task.result_key).await?;
        assert_eq!(hint.bounds, measured.bounds);
    }
    let raster_store = aqw_component_raster::storage::FsStore::new(temporary.path().into());
    for index in 0..4 {
        let event = serde_json::from_value(
            json!({"job_id":JOB,"manifest_key":prepared["manifest_key"],"task_index":index}),
        )?;
        let result =
            aqw_component_raster::worker::run_raster_task(&event, &raster_store, &raster_store)
                .await?;
        assert!(!result.empty);
        assert_eq!(result.render_backend, "resvg");
    }
    let cached = bounds::plan(
        &store,
        "work",
        JOB,
        "jobs/input.json",
        BTreeMap::from([(0, "vector-manifests/fixture.json".into())]),
        ProbeConfig::new(1.0),
        bounds::PlanOptions::new(true, bounds::BoundsMode::Inline),
    )
    .await?;
    assert_eq!(cached["task_count"], 0);
    let complete: Value = store::read(
        &store,
        "work",
        &format!("jobs/{JOB}/prepare/bounds-complete.json"),
    )
    .await?;
    assert_eq!(complete["states"].as_object().unwrap().len(), 2);
    Ok(())
}

#[tokio::test]
async fn invisible_cape_has_no_phantom_canvas_but_faint_cape_stays_conservative() -> Result<()> {
    let temporary = tempfile::tempdir()?;
    let store = FsStore(temporary.path().into());
    let (request, _) = synthetic(&store).await?;
    let mut source: SourceManifest = store::read(&store, "work", "vector-manifests/fixture.json").await?;
    let mut input: Value = store::read(&store, "work", "jobs/input.json").await?;
    let mut canvases = Vec::new();
    for mode in ["invisible", "absent", "faint"] {
        let body = if mode == "faint" { r#"<rect width=".000001" height=".000001" opacity=".000001"/>"# } else { r#"<g opacity="0"><rect width="1400" height="1500"/></g>"# };
        let bytes = format!(r#"<svg xmlns="http://www.w3.org/2000/svg" width="1431" height="1566"><g transform="matrix(1 0 0 1 0 0)">{body}</g></svg>"#).into_bytes();
        let state = StateRef::new(&bytes);
        source.symbols.get_mut("cape").unwrap().schedule.fill(state.sha256.clone());
        source.states.insert(state.sha256.clone(), state.clone());
        store.put("work", &state.svg_key, bytes, "image/svg+xml", true).await?;
        store::write(&store, "work", "source.json", &source, false).await?;
        if mode == "absent" { input["aliases"].as_object_mut().unwrap().remove("cape"); }
        else { input["aliases"]["cape"] = "cape".into(); }
        store::write(&store, "work", "jobs/input.json", &input, false).await?;
        let planned = bounds::plan(&store, "work", JOB, "jobs/input.json", BTreeMap::from([(0,"source.json".into())]), ProbeConfig::new(1.0), bounds::PlanOptions::new(true, bounds::BoundsMode::Inline)).await?;
        let tasks: Vec<ProbeTask> = store::read(&store, "work", planned["tasks_key"].as_str().unwrap()).await?;
        for task in tasks { bounds::run_probe(&store, "work", &task).await?; }
        let prepared = finish::finish(&store, &config(), &json!({"request":request,"input_key":"jobs/input.json","bounds_plan_key":planned["plan_key"]})).await?;
        let manifest: Value = store::read(&store, "work", prepared["manifest_key"].as_str().unwrap()).await?;
        canvases.push(manifest["viewbox"].clone());
        if mode == "invisible" {
            let (index, task) = manifest["component_tasks"].as_array().unwrap().iter().enumerate().find(|(_,t)| t["symbol_key"] == "cape").unwrap();
            assert!(task["raster_bounds"]["bounds"].is_null());
            let raster = aqw_component_raster::storage::FsStore::new(temporary.path().into());
            let result = aqw_component_raster::worker::run_raster_task(&serde_json::from_value(json!({"job_id":JOB,"manifest_key":prepared["manifest_key"],"task_index":index}))?, &raster, &raster).await?;
            assert!(result.empty);
            assert_eq!(result.rasterize_ms, 0.0);
        }
    }
    assert_eq!(canvases[0], canvases[1], "invisible artwork must not enlarge the canvas");
    assert!(canvases[2][2].as_f64().unwrap() > canvases[0][2].as_f64().unwrap(), "unresolved faint artwork must retain conservative framing");
    Ok(())
}

#[tokio::test]
async fn cache_bypass_uses_job_scoped_bounds_and_explicit_fanout_modes() -> Result<()> {
    let temporary = tempfile::tempdir()?;
    let store = FsStore(temporary.path().into());
    let (_, initial) = synthetic(&store).await?;
    let initial_tasks: Vec<ProbeTask> =
        store::read(&store, "work", initial["tasks_key"].as_str().unwrap()).await?;
    for task in &initial_tasks {
        bounds::run_probe(&store, "work", task).await?;
    }

    let uncached = bounds::plan(
        &store,
        "work",
        JOB,
        "jobs/input.json",
        BTreeMap::from([(0, "vector-manifests/fixture.json".into())]),
        ProbeConfig::new(1.0),
        bounds::PlanOptions::new(false, bounds::BoundsMode::Inline),
    )
    .await?;
    assert_eq!(uncached["task_count"], 2);
    assert_eq!(uncached["bounds_mode"], "inline");
    let tasks: Vec<ProbeTask> = serde_json::from_value(uncached["inline_tasks"].clone())?;
    assert!(tasks.iter().all(|task| {
        !task.cache_enabled
            && task
                .result_key
                .starts_with(&format!("jobs/{JOB}/prepare/bounds-results/"))
    }));
    for task in &tasks {
        bounds::run_probe(&store, "work", task).await?;
        assert!(store.exists("work", &task.result_key).await?);
    }

    let distributed = bounds::plan(
        &store,
        "work",
        JOB,
        "jobs/input.json",
        BTreeMap::from([(0, "vector-manifests/fixture.json".into())]),
        ProbeConfig::new(1.0),
        bounds::PlanOptions::new(false, bounds::BoundsMode::Distributed),
    )
    .await?;
    assert_eq!(distributed["bounds_mode"], "distributed");
    // Cache bypass forbids cross-job reuse, but the exporter prefetch and the
    // barrier share this job-scoped result identity.
    assert_eq!(distributed["task_count"], 0);
    assert!(distributed["inline_tasks"].is_null());
    Ok(())
}

/// Run explicitly with the exact incident asset and pinned FFDec, neither of
/// which is checked into this repository. The first call has cold caches; a
/// warm call intentionally points at an absent jar to prove FFDec is skipped.
#[tokio::test]
#[ignore = "requires AQW_TEST_SWF and AQW_TEST_FFDEC"]
async fn incident_ground_cold_export_and_warm_cache() -> Result<()> {
    let bytes = std::fs::read(std::env::var("AQW_TEST_SWF")?)?;
    assert_eq!(
        pipeline::sha256(&bytes),
        "ce557052b3effab8fda152c7b4aee7769207da45e90c868c44d5b0e6950ae10c"
    );
    let temporary = tempfile::tempdir()?;
    let store = FsStore(temporary.path().into());
    store
        .put(
            "source",
            "ground.swf",
            bytes.clone(),
            "application/x-shockwave-flash",
            true,
        )
        .await?;
    let source = json!({"idx":0,"key":"ground.swf","sha256":pipeline::sha256(&bytes),"requests":[{"key":"ground","class_name":"LaeDWearNeonDragon","character_id":121,"frame":1,"root_timeline_frames":1}]});
    store::write(&store,"work","jobs/input.json",&json!({"job_id":JOB,"settings":{"zoom":1.0,"subframe_start":1},"export_frame_count":128,"sources":[source]}),false).await?;
    let event = json!({"job_id":JOB,"input_key":"jobs/input.json","source":source});
    let exported = pipeline::export::export_source(
        &store,
        "work",
        "source",
        &event,
        pipeline::export::ExportOptions::without_prefetch(config().ffdec, Duration::from_secs(280)),
    )
    .await?;
    let manifest: SourceManifest =
        store::read(&store, "work", exported["manifest_key"].as_str().unwrap()).await?;
    assert_eq!(manifest.states.len(), 104);
    assert_eq!(manifest.symbols["ground"].schedule.len(), 128);
    assert_eq!(manifest.symbols["ground"].mirror_flip_frame, 49);
    assert_eq!(manifest.symbols["ground"].animated_span, 49);
    assert!(!temporary.path().join("work/svg-bounds").exists());
    let warm = pipeline::export::export_source(
        &store,
        "work",
        "source",
        &event,
        pipeline::export::ExportOptions::without_prefetch(
            PathBuf::from("/nonexistent/ffdec.jar"),
            Duration::from_secs(30),
        ),
    )
    .await?;
    assert_eq!(warm["vector_cache_hit"], true);
    // Exercise actual complex nested/filter SVGs using patched resvg. Exact
    // SVG hashing remains authoritative; no visual-thumbnail dedup is used.
    for state in manifest.states.values().step_by(16) {
        let result = bounds::run_probe(
            &store,
            "work",
            &ProbeTask::new(state.clone(), ProbeConfig::new(1.0))?,
        )
        .await?;
        assert!(result.bounds.is_some());
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires AQW_TEST_CWEBP and CHAR_RENDER_WEBPMUX"]
async fn full_rust_pipeline_encodes_and_validates_webp() -> Result<()> {
    for frame_count in [1, 8] {
        let temporary = tempfile::tempdir()?;
        let store = FsStore(temporary.path().into());
        // Force both unique recipes in the eight-frame fixture into one
        // adaptive batch, exercising upload/encode overlap before final mux.
        let mut test_config = config();
        test_config.compose_concurrency = 1;
        let (mut request, planned) = synthetic(&store).await?;
        if frame_count == 1 {
            request["render"]["complete_loop"] = false.into();
            let mut input: Value = store::read(&store, "work", "jobs/input.json").await?;
            input["settings"] = request["render"].clone();
            store::write(&store, "work", "jobs/input.json", &input, false).await?;
        }
        let tasks: Vec<ProbeTask> =
            store::read(&store, "work", planned["tasks_key"].as_str().unwrap()).await?;
        for task in tasks {
            bounds::run_probe(&store, "work", &task).await?;
        }
        let prepared=finish::finish(&store,&test_config,&json!({"request":request,"input_key":"jobs/input.json","bounds_plan_key":planned["plan_key"]})).await?;
        let raster_store = aqw_component_raster::storage::FsStore::new(temporary.path().into());
        let mut results = Vec::new();
        for index in prepared["component_task_indices"].as_array().unwrap() {
            results.push(serde_json::to_value(aqw_component_raster::worker::run_raster_task(&serde_json::from_value(json!({"job_id":JOB,"manifest_key":prepared["manifest_key"],"task_index":index}))?,&raster_store,&raster_store).await?)?);
        }
        let job = prepared["job_id"].as_str().unwrap();
        let handoff = pipeline::components::collect(&store, "work", &json!({"job_id":job,"manifest_key":prepared["manifest_key"]})).await?;
        let compose_store = ComposeStore(&store);
        let opts = aqw_component_compose::worker::ComposeOptions {
            cwebp: std::env::var("AQW_TEST_CWEBP")?.into(),
            download_concurrency: 4,
            scratch_dir: temporary.path().into(),
            retain_png_dir: None,
        };
        let mut rendered = Vec::new();
        for batch in prepared["component_batches"]
            .as_array()
            .context("missing batches")?
        {
            let event = serde_json::from_value(
                json!({"job_id":job,"manifest_key":prepared["manifest_key"],"component_results_key":handoff["manifest_key"],"batch":batch}),
            )?;
            rendered.push(serde_json::to_value(
                aqw_component_compose::worker::run_chunk(
                    &event,
                    &compose_store,
                    &compose_store,
                    &opts,
                )
                .await?
                .0,
            )?);
        }
        let finalized = pipeline::finalize::finalize(
        &store,
        &test_config,
        &json!({"job_id":job,"manifest_key":prepared["manifest_key"],"render_results":rendered}),
    )
    .await?;
        assert_eq!(finalized["frame_count"], frame_count);
        assert_eq!(finalized["logical_frame_count"], frame_count);
        assert!(finalized["physical_frame_count"].as_u64().unwrap() <= frame_count);
        assert!(finalized["bytes"].as_u64().unwrap() > 0);
        // Missing and duplicated encoded batches fail before publishing a result.
        assert!(pipeline::finalize::finalize(
            &store,
            &test_config,
            &json!({"job_id":job,"manifest_key":prepared["manifest_key"],"render_results":[]})
        )
        .await
        .is_err());
        rendered.push(rendered[0].clone());
        assert!(pipeline::finalize::finalize(
            &store,
            &test_config,
            &json!({"job_id":job,"manifest_key":prepared["manifest_key"],"render_results":rendered})
        )
        .await
        .is_err());
    }
    Ok(())
}

async fn assert_same_webp_timeline(reference: &std::path::Path, candidate: &std::path::Path) -> Result<()> {
    let result = tokio::process::Command::new(std::env::var("AQW_TEST_PYTHON").context("set AQW_TEST_PYTHON to a Python with Pillow")?)
        .arg(concat!(env!("CARGO_MANIFEST_DIR"),"/tests/verify_webp_timeline.py"))
        .arg(reference).arg(candidate).output().await?;
    anyhow::ensure!(result.status.success(),"decoded timeline differs: {}",String::from_utf8_lossy(&result.stderr));
    println!("{}",String::from_utf8_lossy(&result.stdout).trim());
    Ok(())
}

async fn finalize_test_records(store: &FsStore, records: &[Value]) -> Result<Value> {
    let prepared = json!({"job_id":JOB,"frame_count":records.len(),"frame_durations":records.iter().map(|r|r["duration"].clone()).collect::<Vec<_>>(),"final_key":"renders/merged.webp","render_hash":"fixture"});
    store::write(store,"work","prepare.json",&prepared,false).await?;
    // Reversed/out-of-order batches verify that runs follow logical order and
    // cross batch boundaries, not completion order or canonical-frame order.
    let reversed: Vec<_> = records.iter().rev().cloned().collect();
    let mut results = Vec::new();
    for (index, chunk) in reversed.chunks(3).enumerate() {
        let key = format!("batch-{index}.json");
        store::write(store,"work",&key,&json!({"job_id":JOB,"frames":chunk}),false).await?;
        results.push(json!({"batch_manifest_key":key}));
    }
    pipeline::finalize::finalize(store,&config(),&json!({"job_id":JOB,"manifest_key":"prepare.json","render_results":results})).await
}

#[tokio::test]
#[ignore = "requires AQW_TEST_CWEBP, CHAR_RENDER_WEBPMUX, and AQW_TEST_PYTHON with Pillow"]
async fn adjacent_runs_preserve_decoded_pixels_timing_and_static_animation() -> Result<()> {
    let root = tempfile::tempdir()?;
    for lossless in [false,true] {
        let mut images = Vec::new();
        for variant in 0..2 {
            let pixels: Vec<u8> = (0..64).flat_map(|n|[(40+variant*80) as u8,(n*3) as u8,180,if n%3==0 {0} else {128}]).collect();
            let png = aqw_component_compose::png::encode_rgba8(8,8,&pixels)?;
            let png_path = root.path().join(format!("{lossless}-{variant}.png"));
            let webp_path = png_path.with_extension("webp");
            tokio::fs::write(&png_path,png).await?;
            let mut cmd = tokio::process::Command::new(std::env::var("AQW_TEST_CWEBP")?);
            cmd.args(["-quiet","-q","85","-m","4"]);
            if lossless { cmd.args(["-lossless","-exact"]); }
            let output = cmd.arg(&png_path).arg("-o").arg(&webp_path).output().await?;
            anyhow::ensure!(output.status.success(),"cwebp failed: {}",String::from_utf8_lossy(&output.stderr));
            images.push((webp_path.clone(),tokio::fs::read(webp_path).await?));
        }
        for (case, variants, durations, expected_count) in [
            ("still",vec![0],vec![42],1),
            ("constant",vec![0,0,0,0],vec![41,42,41,42],1),
            ("adjacent",vec![0,0,1,1,0],vec![41,42,41,42,41],3),
            ("nonadjacent",vec![0,1,0],vec![41,42,41],3),
            ("limit",vec![0,0],vec![0xffffff-19,20],2),
            ("short",vec![0,0],vec![5,5],2),
        ] {
            let case_root = root.path().join(format!("{lossless}-{case}"));
            let store = FsStore(case_root.clone());
            let mut records = Vec::new();
            let mut mux = tokio::process::Command::new(std::env::var("CHAR_RENDER_WEBPMUX")?);
            for (index, (&variant, &duration)) in variants.iter().zip(&durations).enumerate() {
                let (path, bytes) = &images[variant];
                // Different keys for identical bytes exercise verified content identity.
                let key = format!("frames/{index}.webp");
                store.put("work",&key,bytes.clone(),"image/webp",false).await?;
                records.push(json!({"frame":index+1,"webp_key":key,"sha256":pipeline::sha256(bytes),"bytes":bytes.len(),"x":0,"y":0,"width":8,"height":8,"canvas_width":8,"canvas_height":8,"duration":duration}));
                mux.arg("-frame").arg(path).arg(format!("+{duration}+0+0+0-b"));
            }
            let baseline = case_root.join("baseline.webp");
            let output = mux.args(["-loop","0","-bgcolor","0,0,0,0","-o"]).arg(&baseline).output().await?;
            anyhow::ensure!(output.status.success(),"baseline mux failed: {}",String::from_utf8_lossy(&output.stderr));
            let result = finalize_test_records(&store,&records).await?;
            assert_eq!(result["frame_count"],records.len());
            assert_eq!(result["physical_frame_count"],expected_count);
            assert_eq!(result["duration_ms"],durations.iter().map(|d|*d as u64).sum::<u64>());
            let candidate = case_root.join("work/renders/merged.webp");
            assert_same_webp_timeline(&baseline,&candidate).await?;
            if expected_count < records.len() { assert!(std::fs::metadata(&candidate)?.len() < std::fs::metadata(&baseline)?.len()); }
        }
        // Bad payload metadata must fail before any final image/cache publication.
        for bad in ["checksum","length","dimensions","missing","metadata","animated"] {
            let store = FsStore(root.path().join(format!("{lossless}-{bad}")));
            let mut bytes = images[0].1.clone();
            let mut record = json!({"frame":1,"webp_key":"bad.webp","sha256":pipeline::sha256(&bytes),"bytes":bytes.len(),"x":0,"y":0,"width":8,"height":8,"canvas_width":8,"canvas_height":8,"duration":42});
            if bad == "metadata" {
                bytes.extend_from_slice(b"EXIF\x02\0\0\0{}");
                let size = (bytes.len()-8) as u32;
                bytes[4..8].copy_from_slice(&size.to_le_bytes());
            } else if bad == "animated" {
                bytes = pipeline::webp::single_frame_animation(&bytes,[8,8],pipeline::webp::FrameInfo{x:0,y:0,width:8,height:8,duration:42,flags:2})?;
            }
            if matches!(bad,"metadata"|"animated") { record["sha256"]=pipeline::sha256(&bytes).into();record["bytes"]=bytes.len().into(); }
            match bad { "checksum"=>bytes[15]^=1,"length"=>record["bytes"]=(bytes.len()+1).into(),"dimensions"=>{record["width"]=6.into();record["canvas_width"]=6.into();},_=>() }
            if bad != "missing" { store.put("work","bad.webp",bytes,"image/webp",false).await?; }
            assert!(finalize_test_records(&store,&[record]).await.is_err());
            assert!(!store.exists("work","renders/merged.webp").await?);
            assert!(!store.exists("work","renders/merged.webp.json").await?);
        }
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires AQW_TEST_RUN_SOURCE, CHAR_RENDER_WEBPMUX and AQW_TEST_PYTHON; optional AQW_TEST_RUN_OUTPUT"]
async fn adjacent_runs_on_saved_animation() -> Result<()> {
    let source = PathBuf::from(std::env::var("AQW_TEST_RUN_SOURCE")?);
    let original = tokio::fs::read(&source).await?;
    let info = pipeline::webp::inspect(&original)?;
    anyhow::ensure!(info.loop_count == Some(0) && info.background == Some(0),"fixture must use current loop/background semantics");
    let root = tempfile::tempdir()?;
    let store = FsStore(root.path().into());
    let mut records = Vec::new();
    for (index, frame) in info.frames.iter().enumerate() {
        anyhow::ensure!(frame.flags == pipeline::webp::REPLACE_NO_DISPOSE,"fixture must use no-blend/no-dispose frames");
        let path = root.path().join(format!("frame-{index}.webp"));
        let output = tokio::process::Command::new(std::env::var("CHAR_RENDER_WEBPMUX")?)
            .args(["-get","frame",&(index+1).to_string()]).arg(&source).arg("-o").arg(&path).output().await?;
        anyhow::ensure!(output.status.success(),"frame extraction failed: {}",String::from_utf8_lossy(&output.stderr));
        let bytes = tokio::fs::read(path).await?;
        let sha = pipeline::sha256(&bytes);
        let key = format!("frames/{sha}.webp");
        store.put("work",&key,bytes.clone(),"image/webp",true).await?;
        records.push(json!({"frame":index+1,"webp_key":key,"sha256":sha,"bytes":bytes.len(),"x":frame.x,"y":frame.y,"width":frame.width,"height":frame.height,"canvas_width":info.canvas[0],"canvas_height":info.canvas[1],"duration":frame.duration}));
    }
    let result = finalize_test_records(&store,&records).await?;
    let candidate = root.path().join("work/renders/merged.webp");
    assert_same_webp_timeline(&source,&candidate).await?;
    assert_eq!(result["frame_count"],info.frames.len());
    if let Ok(path) = std::env::var("AQW_TEST_RUN_OUTPUT") {
        tokio::fs::copy(&candidate,&path).await?;
        tokio::fs::write(format!("{path}.json"),serde_json::to_vec_pretty(&result)?).await?;
    }
    println!("saved animation: before_bytes={}, result={result}",original.len());
    Ok(())
}

/// Local-only replay against downloaded incident sources. Keeps artifacts in
/// AQW_TEST_STORE so the rendered animation and manifests can be inspected.
#[tokio::test]
#[ignore = "requires downloaded incident input/sources, FFDec, cwebp and webpmux"]
async fn replay_incident_through_all_rust_stages() -> Result<()> {
    let root = PathBuf::from(std::env::var("AQW_TEST_STORE")?);
    let store = FsStore(root.clone());
    let original: Value = serde_json::from_slice(&std::fs::read(root.join("input.json"))?)?;
    let mut assets = serde_json::Map::new();
    for source in original["sources"]
        .as_array()
        .context("missing original sources")?
    {
        let mut record = source.clone();
        record["size"] = json!(std::fs::metadata(
            root.join("source").join(source["key"].as_str().unwrap())
        )?
        .len());
        assets.insert(source["remote_path"].as_str().unwrap().into(), record);
    }
    let mut character = original["character_renderer"].clone();
    character["size"] = json!(std::fs::metadata(
        root.join("source").join(character["key"].as_str().unwrap())
    )?
    .len());
    store::write(&store,"source","datasets/dev-v1/manifest.json",&json!({"schema_version":1,"dataset_version":"dev-v1","assets":assets,"character_renderer":character}),false).await?;
    let mut settings = original["settings"].clone();
    settings["raster_backend"] = "resvg".into();
    settings["raster_size"] = 512.into();
    settings["output_size"] = 256.into();
    let request = pipeline::contract::request(
        json!({"schema_version":1,"job_id":original["job_id"],"created_at":"2026-09-05T00:00:00Z","discord":{"user_id":"1","channel_id":"2"},"render":settings,"appearance":original["fields"]}),
        None,
    )?;
    let mut config = config();
    config.component_frame_cap = 12;
    let resolved = pipeline::resolve::resolve(&store, &config, &request).await?;
    let input_key = resolved["input_key"].as_str().unwrap();
    let prepared: Value = store::read(&store, "work", input_key).await?;
    assert_eq!(prepared["export_frame_count"], 128);
    assert_eq!(prepared["aliases"], original["aliases"]);
    for source in prepared["sources"].as_array().unwrap() {
        let expected = original["sources"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["sha256"] == source["sha256"])
            .context("unexpected resolved source")?;
        assert_eq!(source["requests"], expected["requests"]);
    }
    let job = request["job_id"].as_str().unwrap();
    let mut sources = BTreeMap::new();
    for source in resolved["sources"].as_array().unwrap() {
        let exported = pipeline::export::export_source(
            &store,
            "work",
            "source",
            &json!({"job_id":job,"input_key":input_key,"source":source}),
            pipeline::export::ExportOptions::without_prefetch(
                config.ffdec.clone(),
                Duration::from_secs(280),
            ),
        )
        .await?;
        sources.insert(
            exported["source_idx"].as_u64().unwrap() as usize,
            exported["manifest_key"].as_str().unwrap().to_string(),
        );
    }
    let planned = bounds::plan(
        &store,
        "work",
        job,
        input_key,
        sources.clone(),
        ProbeConfig::new(1.0),
        bounds::PlanOptions::new(true, bounds::BoundsMode::Inline),
    )
    .await?;
    let tasks: Vec<ProbeTask> =
        store::read(&store, "work", planned["tasks_key"].as_str().unwrap()).await?;
    for task in tasks {
        bounds::run_probe(&store, "work", &task).await?;
    }
    let finished = finish::finish(
        &store,
        &config,
        &json!({"request":request,"input_key":input_key,"bounds_plan_key":planned["plan_key"]}),
    )
    .await?;
    let manifest_key = finished["manifest_key"].as_str().unwrap();
    let raster_store = aqw_component_raster::storage::FsStore::new(root.clone());
    let mut results = Vec::new();
    for index in finished["component_task_indices"].as_array().unwrap() {
        results.push(serde_json::to_value(
            aqw_component_raster::worker::run_raster_task(
                &serde_json::from_value(
                    json!({"job_id":job,"manifest_key":manifest_key,"task_index":index}),
                )?,
                &raster_store,
                &raster_store,
            )
            .await?,
        )?);
    }
    let compose_store = ComposeStore(&store);
    let temporary = tempfile::tempdir()?;
    let opts = aqw_component_compose::worker::ComposeOptions {
        cwebp: std::env::var("AQW_TEST_CWEBP")?.into(),
        download_concurrency: 4,
        scratch_dir: temporary.path().into(),
        retain_png_dir: Some(root.join("preview-png")),
    };
    let mut rendered = Vec::new();
    for batch in finished["component_batches"].as_array().unwrap() {
        rendered.push(serde_json::to_value(aqw_component_compose::worker::run_chunk(&serde_json::from_value(json!({"job_id":job,"manifest_key":manifest_key,"component_results":results,"batch":batch}))?,&compose_store,&compose_store,&opts).await?.0)?);
    }
    let final_event = json!({"job_id":job,"manifest_key":manifest_key,"render_results":rendered});
    store::write(
        &store,
        "work",
        "replay-finalize-event.json",
        &final_event,
        false,
    )
    .await?;
    let result = pipeline::finalize::finalize(&store, &config, &final_event).await?;
    assert_eq!(result["frame_count"], 12);
    assert!(result["bytes"].as_u64().unwrap() > 0);
    let warm = bounds::plan(
        &store,
        "work",
        job,
        input_key,
        sources,
        ProbeConfig::new(1.0),
        bounds::PlanOptions::new(true, bounds::BoundsMode::Inline),
    )
    .await?;
    assert_eq!(warm["task_count"], 0);
    println!("incident_replay_result={result}");
    Ok(())
}
