//! Raster metadata stays in S3, never in a Map's accumulated result array.
use crate::{
    contract::string,
    store::{self, Store},
};
use anyhow::{ensure, Context, Result};
use futures::{stream, StreamExt, TryStreamExt};
use serde_json::{json, Value};
use std::{collections::BTreeSet, time::Instant};

fn job_id(value: &str) -> Result<&str> {
    ensure!(
        uuid::Uuid::parse_str(value)?.to_string() == value,
        "invalid job UUID"
    );
    Ok(value)
}

/// Verify every expected result, compact it once, and publish one S3 handoff.
/// No results or rasters from another job are copied. Work is bounded.
pub async fn collect(store: &dyn Store, bucket: &str, event: &Value) -> Result<Value> {
    let started = Instant::now();
    let job = job_id(string(event, "job_id")?)?;
    let key = string(event, "manifest_key")?;
    let manifest: Value = store::read(store, bucket, key).await?;
    ensure!(
        manifest["job_id"] == job && manifest["component_pipeline"] == true,
        "component prepare manifest mismatch"
    );
    let tasks = manifest["component_tasks"]
        .as_array()
        .context("missing component tasks")?;
    ensure!(!tasks.is_empty(), "empty component tasks");
    let mut seen = BTreeSet::new();
    for task in tasks {
        let id = string(task, "task_id")?;
        ensure!(
            !id.is_empty()
                && id
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c)),
            "unsafe component task ID"
        );
        ensure!(seen.insert(id), "duplicate component task ID");
    }
    let records: Vec<Value> = stream::iter(tasks.iter()).map(|task| {
        let manifest = &manifest;
        async move {
            let id = string(task, "task_id")?;
            let result_key = format!("jobs/{job}/component/results/{id}.json");
            let result: Value = store::read(store, bucket, &result_key).await
                .with_context(|| format!("Component {id} is unavailable; all raster tasks must complete before collection"))?;
            ensure!(result["task_id"] == id && result["result_key"] == result_key, "component result identity mismatch for {id}");
            ensure!(result["empty"].is_boolean(), "invalid empty flag for {id}");
            if result["empty"] != true {
                ensure!(result["x"].as_i64().is_some() && result["y"].as_i64().is_some(), "missing placement for {id}");
                ensure!(result["component_raster_space"] == manifest["component_raster_space"], "component coordinate space mismatch for {id}");
                let png_key = format!("jobs/{job}/component/rasters/{id}.png");
                ensure!(string(&result, "png_key")? == png_key, "unexpected PNG key for {id}");
                let sha = string(&result, "sha256")?;
                ensure!(sha.len() == 64 && sha.bytes().all(|c| c.is_ascii_hexdigit()), "invalid PNG checksum for {id}");
            }
            let fields = ["task_id", "empty", "png_key", "sha256", "x", "y", "component_raster_space"];
            let compact: serde_json::Map<String, Value> = fields.into_iter()
                .filter_map(|name| result.get(name).map(|value| (name.into(), value.clone()))).collect();
            Ok::<_, anyhow::Error>(Value::Object(compact))
        }
    }).buffered(16).try_collect().await?;
    let result_key = format!("jobs/{job}/component/manifest.json");
    store::write(
        store,
        bucket,
        &result_key,
        &json!({
            "schema_version":1,"job_id":job,"prepare_manifest_key":key,"component_results":records
        }),
        false,
    )
    .await?;
    crate::log(
        "collect_components_profile",
        json!({"job_id":job,"task_count":records.len(),"duration_ms":started.elapsed().as_secs_f64()*1000.0}),
    );
    Ok(json!({"manifest_key":result_key,"task_count":records.len()}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{contract, store::FsStore};
    const JOB: &str = "45cfafbd-5089-4f6d-850a-caa798ec1fcb";

    async fn fixture(store: &dyn Store, count: usize) -> Result<Value> {
        let request = contract::request(
            json!({"schema_version":1,"job_id":JOB,"created_at":"2026-09-05T00:00:00Z","discord":{"user_id":"1","channel_id":"2"},"render":{"username":"Test"}}),
            None,
        )?;
        let manifest = json!({"job_id":JOB,"component_pipeline":true,"settings":request["render"],"component_raster_space":"output","component_batches":[{"index":0,"composition_start":0,"composition_end":0}],"component_tasks":(0..count).map(|i|json!({"task_id":format!("t{i}")})).collect::<Vec<_>>()});
        store::write(
            store,
            "work",
            &format!("jobs/{JOB}/prepare/manifest.json"),
            &manifest,
            false,
        )
        .await?;
        for i in 0..count {
            let key = format!("jobs/{JOB}/component/results/t{i}.json");
            store::write(store, "work", &key, &json!({"task_id":format!("t{i}"),"empty":true,"result_key":key,"profiling": "x".repeat(1024)}), false).await?;
        }
        Ok(request)
    }
    fn event(job: &str) -> Value {
        json!({"job_id":job,"manifest_key":format!("jobs/{job}/prepare/manifest.json")})
    }

    #[tokio::test]
    async fn large_map_handoff_stays_small_and_missing_results_fail() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let store = FsStore(dir.path().into());
        fixture(&store, 418).await?;
        let output = collect(&store, "work", &event(JOB)).await?;
        assert!(serde_json::to_vec(&output)?.len() < 200);
        let saved: Value = store::read(&store, "work", string(&output, "manifest_key")?).await?;
        assert_eq!(saved["component_results"].as_array().unwrap().len(), 418);
        assert!(saved["component_results"][0].get("profiling").is_none());
        std::fs::remove_file(
            dir.path()
                .join(format!("work/jobs/{JOB}/component/results/t0.json")),
        )?;
        assert!(collect(&store, "work", &event(JOB)).await.is_err());
        Ok(())
    }

    #[tokio::test]
    async fn collector_validates_references_and_preserves_raster_records() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let store = FsStore(dir.path().into());
        fixture(&store, 2).await?;
        let bytes = b"test raster bytes".to_vec();
        let png = format!("jobs/{JOB}/component/rasters/t0.png");
        let key = format!("jobs/{JOB}/component/results/t0.json");
        let record = json!({"task_id":"t0","empty":false,"png_key":png,"sha256":crate::sha256(&bytes),"x":1,"y":2,"component_raster_space":"output","result_key":key});
        store
            .put("work", &png, bytes.clone(), "image/png", false)
            .await?;
        store::write(&store, "work", &key, &record, false).await?;
        collect(&store, "work", &event(JOB)).await?;
        assert_eq!(store::read::<Value>(&store, "work", &key).await?, record);
        for (field, value) in [
            ("png_key", json!("jobs/another-job/component/rasters/t0.png")),
            ("component_raster_space", json!("raster")),
            ("result_key", json!("wrong-result.json")),
            ("sha256", json!(null)),
        ] {
            let mut invalid = record.clone();
            invalid[field] = value;
            store::write(&store, "work", &key, &invalid, false).await?;
            assert!(collect(&store, "work", &event(JOB)).await.is_err());
        }
        Ok(())
    }
}
