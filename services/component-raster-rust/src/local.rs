//! Local fixture mode: run one raster task against a filesystem store that
//! mirrors the Python `FilesystemObjectStore` layout (`<root>/work/<key>`).
//! This enables exact parity comparisons with the Python raster worker
//! without mocking S3.

use std::path::PathBuf;

use crate::contract::{RasterEvent, RasterResult};
use crate::error::RasterError;
use crate::storage::FsStore;
use crate::worker::run_raster_task;

pub struct CliOptions {
    pub store_root: PathBuf,
    pub job_id: String,
    pub manifest_key: String,
    pub task_index: i64,
    pub benchmark_output_prefix: Option<String>,
}

fn usage() -> &'static str {
    "\
usage: aqw-component-raster local-raster --store-root DIR --job-id JOB --task-index N \
[--manifest-key KEY] [--benchmark-output-prefix PREFIX]"
}

pub fn parse_cli(args: &[String]) -> Result<CliOptions, RasterError> {
    if args.is_empty() || args[0] != "local-raster" {
        return Err(RasterError::invalid(usage().to_string()));
    }
    let mut store_root: Option<PathBuf> = None;
    let mut job_id: Option<String> = None;
    let mut manifest_key: Option<String> = None;
    let mut task_index: Option<i64> = None;
    let mut benchmark_output_prefix: Option<String> = None;

    let mut index = 1;
    while index < args.len() {
        let flag = args[index].as_str();
        let value = args
            .get(index + 1)
            .ok_or_else(|| RasterError::invalid(format!("{flag} requires a value\n{}", usage())))?;
        match flag {
            "--store-root" => store_root = Some(PathBuf::from(value)),
            "--job-id" => job_id = Some(value.clone()),
            "--manifest-key" => manifest_key = Some(value.clone()),
            "--task-index" => {
                task_index = Some(
                    value
                        .parse()
                        .map_err(|_| RasterError::invalid(format!("{flag} must be an integer")))?,
                );
            }
            "--benchmark-output-prefix" => benchmark_output_prefix = Some(value.clone()),
            _ => {
                return Err(RasterError::invalid(format!(
                    "unknown argument {flag}\n{}",
                    usage()
                )));
            }
        }
        index += 2;
    }

    let job_id =
        job_id.ok_or_else(|| RasterError::invalid(format!("--job-id is required\n{}", usage())))?;
    let task_index = task_index
        .ok_or_else(|| RasterError::invalid(format!("--task-index is required\n{}", usage())))?;
    let store_root = store_root
        .ok_or_else(|| RasterError::invalid(format!("--store-root is required\n{}", usage())))?;
    let manifest_key =
        manifest_key.unwrap_or_else(|| format!("jobs/{job_id}/prepare/manifest.json"));
    Ok(CliOptions {
        store_root,
        job_id,
        manifest_key,
        task_index,
        benchmark_output_prefix,
    })
}

pub async fn run_local(cli: &CliOptions) -> Result<RasterResult, RasterError> {
    let root = std::fs::canonicalize(&cli.store_root).map_err(|error| {
        RasterError::invalid(format!(
            "cannot access store root {}: {error}",
            cli.store_root.display()
        ))
    })?;
    let store = FsStore::new(root);
    let event = RasterEvent {
        job_id: cli.job_id.clone(),
        manifest_key: cli.manifest_key.clone(),
        task_index: cli.task_index,
        benchmark_output_prefix: cli.benchmark_output_prefix.clone(),
    };
    let result = run_raster_task(&event, &store, &store).await?;
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(result)
}
