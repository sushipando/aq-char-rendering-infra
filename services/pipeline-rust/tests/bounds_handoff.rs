//! Offline regressions for compact Inline Map inputs and immutable task indices.
use anyhow::Result;
use aqw_render_pipeline::{
    bounds::{self, BoundsMode, PlanOptions},
    model::*,
    store::{self, FsStore, Store},
};
use serde_json::{json, Value};
use std::collections::BTreeMap;

const JOB: &str = "d0f4d42f-e33d-4d59-9073-f6918dc794ab";
const OTHER_JOB: &str = "603b3a9b-48cc-4c3b-9ef5-74a09db2312e";

async fn fixture(store: &FsStore, count: usize) -> Result<()> {
    let mut states = BTreeMap::new();
    for i in 0..count {
        let bytes = format!(r##"<svg xmlns="http://www.w3.org/2000/svg" width="64" height="64"><g transform="matrix(1 0 0 1 0 0)"><rect width="32" height="32" fill="#{i:06x}"/></g></svg>"##).into_bytes();
        let state = StateRef::new(&bytes);
        store
            .put("work", &state.svg_key, bytes, "image/svg+xml", true)
            .await?;
        states.insert(state.sha256.clone(), state);
    }
    let manifest = SourceManifest {
        schema_version: VECTOR_SCHEMA,
        export_policy: EXPORT_POLICY.into(),
        source_sha256: "a".repeat(64),
        export_identity: "fixture".into(),
        symbols: BTreeMap::from([(
            "pet".into(),
            SymbolExport {
                request: SymbolRequest {
                    key: "pet".into(),
                    class_name: "Pet".into(),
                    character_id: 1,
                    frame: 1,
                    root_timeline_frames: 1,
                },
                schedule: states.keys().cloned().collect(),
                mirror_flip_frame: 0,
                random_pose_as3: false,
                animated_span: count,
                settled_stop_frame: None,
            },
        )]),
        states,
        hand_visibility: Default::default(),
        color_rules: BTreeMap::new(),
        placement_colors: BTreeMap::new(),
        timeline_decisions: Vec::new(),
    };
    store::write(store, "work", "source.json", &manifest, false).await?;
    store::write(
        store,
        "work",
        "input.json",
        &json!({
            "job_id": JOB, "sources": [{"idx":0,"sha256":manifest.source_sha256}]
        }),
        false,
    )
    .await
}

async fn plan(store: &FsStore, cache: bool, mode: BoundsMode) -> Result<Value> {
    bounds::plan(
        store,
        "work",
        JOB,
        "input.json",
        BTreeMap::from([(0, "source.json".into())]),
        ProbeConfig::new(1.0),
        PlanOptions::new(cache, mode),
    )
    .await
}

fn event(planned: &Value, index: usize) -> Value {
    json!({"phase":"probe","task_ref":{
        "job_id":JOB,"tasks_key":planned["tasks_key"],"task_index":index
    }})
}

#[tokio::test]
async fn incident_sized_and_large_uncached_plans_keep_inline_payloads_small() -> Result<()> {
    for count in [346, 4000] {
        let temporary = tempfile::tempdir()?;
        let store = FsStore(temporary.path().into());
        fixture(&store, count).await?;
        let planned = plan(&store, false, BoundsMode::Inline).await?;
        let bytes = store
            .get("work", planned["tasks_key"].as_str().unwrap())
            .await?
            .unwrap();
        assert!(
            bytes.len() > 160 * 1024,
            "old full-task inline payload must fail"
        );
        assert_eq!(planned["bounds_mode"], "inline");
        assert_eq!(planned["task_count"], count);
        assert_eq!(
            planned["inline_task_indices"],
            json!((0..count).collect::<Vec<_>>())
        );
        let reply = serde_json::to_vec(&planned)?;
        assert!(reply.len() < 24 * 1024);
        assert!(planned.get("inline_tasks").is_none());
        assert!(!String::from_utf8_lossy(&reply).contains("svg_key"));
        // The CDK iterator trims each result to a scalar before aggregation.
        assert!(serde_json::to_vec(&vec![0; count])?.len() < 160 * 1024);
        assert_eq!(
            bounds::run_inline_probe(&store, "work", &event(&planned, count - 1)).await?,
            json!(0)
        );
        eprintln!(
            "bounds handoff: {count} tasks, {} bytes in S3, {} bytes in PlanBounds reply",
            bytes.len(),
            reply.len()
        );
    }
    Ok(())
}

#[tokio::test]
async fn each_reference_probes_one_svg_and_survives_replanning_and_redelivery() -> Result<()> {
    for cache in [false, true] {
        let temporary = tempfile::tempdir()?;
        let store = FsStore(temporary.path().into());
        fixture(&store, 3).await?;
        let original = plan(&store, cache, BoundsMode::Inline).await?;
        let tasks: Vec<ProbeTask> =
            store::read(&store, "work", original["tasks_key"].as_str().unwrap()).await?;
        let first = event(&original, 0);
        assert_eq!(
            bounds::run_inline_probe(&store, "work", &first).await?,
            json!(0)
        );
        for (i, task) in tasks.iter().enumerate() {
            assert_eq!(store.exists("work", &task.result_key).await?, i == 0);
        }
        let replanned = plan(&store, cache, BoundsMode::Inline).await?;
        assert_ne!(replanned["tasks_key"], original["tasks_key"]);
        assert_eq!(replanned["task_count"], 2);
        // Old index zero still points to the completed task, not a new SVG.
        assert_eq!(
            bounds::run_inline_probe(&store, "work", &first).await?,
            json!(0)
        );
        assert!(!store.exists("work", &tasks[1].result_key).await?);
        // A stale reference can also finish an as-yet unprobed task correctly.
        assert_eq!(
            bounds::run_inline_probe(&store, "work", &event(&original, 2)).await?,
            json!(0)
        );
        assert!(!store.exists("work", &tasks[1].result_key).await?);
        for index in 0..2 {
            bounds::run_inline_probe(&store, "work", &event(&replanned, index)).await?;
        }
        let empty = plan(&store, cache, BoundsMode::Inline).await?;
        assert_eq!(empty["task_count"], 0);
        assert_eq!(empty["inline_task_indices"], json!([]));
    }
    Ok(())
}

#[tokio::test]
async fn distributed_and_legacy_direct_tasks_keep_the_existing_contract() -> Result<()> {
    let temporary = tempfile::tempdir()?;
    let store = FsStore(temporary.path().into());
    fixture(&store, 2).await?;
    let planned = plan(&store, false, BoundsMode::Distributed).await?;
    assert_eq!(planned["bounds_mode"], "distributed");
    assert!(planned["inline_task_indices"].is_null());
    let tasks: Vec<ProbeTask> =
        store::read(&store, "work", planned["tasks_key"].as_str().unwrap()).await?;
    assert_eq!(tasks.len(), 2);
    let legacy = json!({"phase":"probe","task":tasks[0]});
    assert_eq!(
        bounds::run_inline_probe(&store, "work", &legacy).await?,
        json!(0)
    );
    assert!(store.exists("work", &tasks[0].result_key).await?);
    assert!(!store.exists("work", &tasks[1].result_key).await?);
    Ok(())
}

#[tokio::test]
async fn invalid_or_cross_job_references_fail_before_probing() -> Result<()> {
    let temporary = tempfile::tempdir()?;
    let store = FsStore(temporary.path().into());
    fixture(&store, 2).await?;
    let planned = plan(&store, false, BoundsMode::Inline).await?;
    let key = planned["tasks_key"].as_str().unwrap();
    let bytes = store.get("work", key).await?.unwrap();
    let tasks: Vec<ProbeTask> = serde_json::from_slice(&bytes)?;
    let valid = event(&planned, 0);
    for bad_index in [
        json!(-1),
        json!(0.5),
        json!("0"),
        json!(null),
        json!(2),
        json!(u64::MAX),
    ] {
        let mut bad = valid.clone();
        bad["task_ref"]["task_index"] = bad_index;
        assert!(bounds::run_inline_probe(&store, "work", &bad)
            .await
            .is_err());
    }
    for (field, value) in [
        ("job_id", json!(OTHER_JOB)),
        ("job_id", json!("invalid")),
        ("tasks_key", json!("../escape.json")),
        (
            "tasks_key",
            json!(format!("jobs/{JOB}/prepare/bounds-tasks/missing.json")),
        ),
        ("unexpected", json!(true)),
    ] {
        let mut bad = valid.clone();
        bad["task_ref"][field] = value;
        assert!(bounds::run_inline_probe(&store, "work", &bad)
            .await
            .is_err());
    }
    for bad in [
        json!({"phase":"other","task_ref":valid["task_ref"]}),
        json!({"phase":"probe"}),
        json!({"phase":"probe","task_ref":valid["task_ref"],"task":tasks[0]}),
    ] {
        assert!(bounds::run_inline_probe(&store, "work", &bad)
            .await
            .is_err());
    }
    // Even a correctly hashed dataset cannot redirect a job-scoped result.
    let other_task =
        ProbeTask::without_cache(tasks[0].state.clone(), tasks[0].config.clone(), OTHER_JOB)?;
    let other_bytes = serde_json::to_vec(&vec![other_task])?;
    let other_key = format!(
        "jobs/{JOB}/prepare/bounds-tasks/{}.json",
        aqw_render_pipeline::sha256(&other_bytes)
    );
    store
        .put("work", &other_key, other_bytes, "application/json", true)
        .await?;
    let mut bad = valid.clone();
    bad["task_ref"]["tasks_key"] = json!(other_key);
    assert!(bounds::run_inline_probe(&store, "work", &bad)
        .await
        .unwrap_err()
        .to_string()
        .contains("another job"));
    // A changed object at the original address must fail its checksum check.
    store
        .put("work", key, b"[]".to_vec(), "application/json", false)
        .await?;
    assert!(bounds::run_inline_probe(&store, "work", &valid)
        .await
        .unwrap_err()
        .to_string()
        .contains("checksum"));
    for task in tasks {
        assert!(!store.exists("work", &task.result_key).await?);
    }
    Ok(())
}
