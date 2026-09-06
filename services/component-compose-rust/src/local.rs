//! Local fixture mode: read the downloaded production artifact layout
//! (``manifest.json``, ``component/results/*.json``, ``component/rasters/*.png``)
//! and run one whole chunk without mocking S3. Writes lossless frame PNGs,
//! frame WebPs, and a compose-batch manifest beneath the output directory.

use std::path::PathBuf;

use crate::contract::{ComponentResult, ComposeEvent};
use crate::error::ComposeError;
use crate::storage::{Sink, Source};
use crate::worker::{compose_and_report, ComposeOptions};

const LOCAL_PREFIX: &str = "local://";

/// Convert one full raster-worker record to the compact result shape carried
/// by the workflow event (`component_workflow_result`).
fn compact_result(record: &serde_json::Value) -> Result<ComponentResult, ComposeError> {
    let task_id = record
        .get("task_id")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| ComposeError::invalid("component result record has no task_id"))?
        .to_string();
    let empty = record.get("empty").and_then(serde_json::Value::as_bool) == Some(true);
    let mut result = ComponentResult {
        task_id: task_id.clone(),
        empty,
        png_key: None,
        sha256: record
            .get("sha256")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        x: record
            .get("x")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0),
        y: record
            .get("y")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0),
        component_raster_space: record
            .get("component_raster_space")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
    };
    if !empty {
        if record
            .get("png_key")
            .and_then(serde_json::Value::as_str)
            .is_none()
        {
            return Err(ComposeError::invalid(format!(
                "component task {task_id} is not empty but has no png_key"
            )));
        }
        result.png_key = Some(format!("{LOCAL_PREFIX}{task_id}"));
    }
    Ok(result)
}

/// Filesystem source: reads rasters and the manifest from the artifact dir.
pub struct FsSource {
    artifact_dir: PathBuf,
}

impl FsSource {
    pub fn new(artifact_dir: PathBuf) -> Self {
        FsSource { artifact_dir }
    }
}

#[async_trait::async_trait]
impl Source for FsSource {
    async fn read_json(&self, key: &str) -> Result<serde_json::Value, ComposeError> {
        let path = if key == "local://manifest" {
            self.artifact_dir.join("manifest.json")
        } else {
            return Err(ComposeError::invalid(format!(
                "unsupported local key {key}"
            )));
        };
        let bytes = tokio::fs::read(&path).await.map_err(|error| {
            ComposeError::invalid(format!("cannot read {}: {error}", path.display()))
        })?;
        serde_json::from_slice(&bytes).map_err(|error| {
            ComposeError::invalid(format!("invalid JSON in {}: {error}", path.display()))
        })
    }

    async fn fetch_bytes(
        &self,
        key: &str,
        expected_sha256: Option<&str>,
    ) -> Result<Vec<u8>, ComposeError> {
        let task_id = key
            .strip_prefix(LOCAL_PREFIX)
            .ok_or_else(|| ComposeError::invalid(format!("unsupported local key {key}")))?;
        let path = self
            .artifact_dir
            .join("component")
            .join("rasters")
            .join(format!("{task_id}.png"));
        let bytes = tokio::fs::read(&path).await.map_err(|error| {
            ComposeError::invalid(format!("cannot read raster {}: {error}", path.display()))
        })?;
        if let Some(expected) = expected_sha256 {
            let actual = crate::telemetry::sha256_hex(&bytes);
            if actual != expected {
                return Err(ComposeError::invalid(format!(
                    "checksum mismatch for {}: expected {expected}, got {actual}",
                    path.display()
                )));
            }
        }
        Ok(bytes)
    }
}

/// Filesystem sink: writes frames and the batch manifest under output_dir.
pub struct FsSink {
    output_dir: PathBuf,
    batch_index: i64,
}

impl FsSink {
    pub fn new(output_dir: PathBuf, batch_index: i64) -> Self {
        FsSink {
            output_dir,
            batch_index,
        }
    }
}

#[async_trait::async_trait]
impl Sink for FsSink {
    async fn put_webp(
        &self,
        frame_number: i64,
        _key: &str,
        bytes: &[u8],
    ) -> Result<(), ComposeError> {
        let dir = self.output_dir.join("frames");
        tokio::fs::create_dir_all(&dir).await?;
        let path = dir.join(format!("{frame_number:06}.webp"));
        tokio::fs::write(&path, bytes).await?;
        Ok(())
    }

    async fn put_json(&self, _key: &str, value: &serde_json::Value) -> Result<(), ComposeError> {
        tokio::fs::create_dir_all(&self.output_dir).await?;
        let path = self
            .output_dir
            .join(format!("batch-{:04}.json", self.batch_index));
        let mut bytes = serde_json::to_vec_pretty(value)?;
        bytes.push(b'\n');
        tokio::fs::write(&path, bytes).await?;
        Ok(())
    }
}

pub struct CliOptions {
    pub artifact_dir: PathBuf,
    pub output_dir: PathBuf,
    pub frame_start: i64,
    pub frame_end: i64,
    pub batch_index: i64,
    pub cwebp: PathBuf,
    pub download_concurrency: usize,
}

fn usage() -> &'static str {
    "\
usage: aqw-component-compose local-compose --artifact-dir DIR --output-dir DIR \
[--frame-start N] [--frame-end N] [--batch-index N] [--cwebp PATH] \
[--download-concurrency N]"
}

/// Minimal dependency-free CLI parser (fixed small surface).
pub fn parse_cli(args: &[String]) -> Result<CliOptions, ComposeError> {
    if args.is_empty() || args[0] != "local-compose" {
        return Err(ComposeError::invalid(usage().to_string()));
    }
    let mut artifact_dir: Option<PathBuf> = None;
    let mut output_dir: Option<PathBuf> = None;
    let mut frame_start = 1i64;
    let mut frame_end: Option<i64> = None;
    let mut batch_index = 0i64;
    let mut cwebp: Option<PathBuf> = None;
    let mut download_concurrency = 32usize;

    let mut index = 1;
    while index < args.len() {
        let flag = args[index].as_str();
        let value = args.get(index + 1).ok_or_else(|| {
            ComposeError::invalid(format!("{flag} requires a value\n{}", usage()))
        })?;
        let consumed = 2;
        match flag {
            "--artifact-dir" => artifact_dir = Some(PathBuf::from(value)),
            "--output-dir" => output_dir = Some(PathBuf::from(value)),
            "--frame-start" => frame_start = parse_i64(value, flag)?,
            "--frame-end" => frame_end = Some(parse_i64(value, flag)?),
            "--batch-index" => batch_index = parse_i64(value, flag)?,
            "--cwebp" => cwebp = Some(PathBuf::from(value)),
            "--download-concurrency" => download_concurrency = parse_usize(value, flag)?,
            _ => {
                return Err(ComposeError::invalid(format!(
                    "unknown argument {flag}\n{}",
                    usage()
                )));
            }
        }
        index += consumed;
    }

    let artifact_dir = artifact_dir
        .ok_or_else(|| ComposeError::invalid(format!("--artifact-dir is required\n{}", usage())))?;
    let output_dir = output_dir
        .ok_or_else(|| ComposeError::invalid(format!("--output-dir is required\n{}", usage())))?;
    let cwebp = cwebp.unwrap_or_else(|| {
        std::env::var("CHAR_RENDER_CWEBP")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("cwebp"))
    });
    Ok(CliOptions {
        artifact_dir,
        output_dir,
        frame_start,
        frame_end: frame_end.unwrap_or(frame_start),
        batch_index,
        cwebp,
        download_concurrency,
    })
}

fn parse_i64(value: &str, flag: &str) -> Result<i64, ComposeError> {
    value
        .parse::<i64>()
        .map_err(|_| ComposeError::invalid(format!("{flag} must be an integer")))
}

fn parse_usize(value: &str, flag: &str) -> Result<usize, ComposeError> {
    value
        .parse::<usize>()
        .map_err(|_| ComposeError::invalid(format!("{flag} must be a positive integer")))
}

/// Run one whole chunk from the downloaded artifact layout.
pub async fn run_local(cli: &CliOptions) -> Result<serde_json::Value, ComposeError> {
    let artifact_dir = std::fs::canonicalize(&cli.artifact_dir).map_err(|error| {
        ComposeError::invalid(format!(
            "cannot access artifact dir {}: {error}",
            cli.artifact_dir.display()
        ))
    })?;

    let manifest_path = artifact_dir.join("manifest.json");
    let manifest_value: serde_json::Value =
        serde_json::from_slice(&tokio::fs::read(&manifest_path).await.map_err(|error| {
            ComposeError::invalid(format!("cannot read {}: {error}", manifest_path.display()))
        })?)
        .map_err(|error| ComposeError::invalid(format!("invalid manifest: {error}")))?;

    let records_root = artifact_dir.join("component").join("results");
    let mut result_paths: Vec<PathBuf> = Vec::new();
    if records_root.is_dir() {
        let mut entries = tokio::fs::read_dir(&records_root).await.map_err(|error| {
            ComposeError::invalid(format!("cannot list {}: {error}", records_root.display()))
        })?;
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("json") {
                result_paths.push(path);
            }
        }
        result_paths.sort();
    }
    let mut results: Vec<ComponentResult> = Vec::new();
    for path in result_paths {
        let value: serde_json::Value = serde_json::from_slice(&tokio::fs::read(&path).await?)
            .map_err(|error| {
                ComposeError::invalid(format!("invalid {}: {error}", path.display()))
            })?;
        results.push(compact_result(&value)?);
    }

    let frame_count = manifest_value
        .get("frame_count")
        .and_then(serde_json::Value::as_i64)
        .ok_or_else(|| ComposeError::invalid("manifest has no frame_count"))?;
    let frame_end = if cli.frame_end == cli.frame_start && cli.frame_end == 1 {
        // The CLI default is a 1-frame range unless --frame-end is explicit;
        // keep it simple and honor exactly what the caller passed.
        cli.frame_end
    } else {
        cli.frame_end
    };
    let _ = frame_count; // validated inside the worker as well
    let _ = frame_end;

    let job_id = manifest_value
        .get("job_id")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| ComposeError::invalid("manifest has no job_id"))?
        .to_string();

    let event = ComposeEvent {
        job_id,
        manifest_key: "local://manifest".to_string(),
        component_results: results,
        batch: crate::contract::BatchIndex {
            index: cli.batch_index,
            frame_start: Some(cli.frame_start),
            frame_end: Some(cli.frame_end),
            composition_start: None,
            composition_end: None,
        },
        benchmark_output_prefix: None,
    };

    let source = FsSource::new(artifact_dir.clone());
    let scratch = scratch_dir()?;
    let options = ComposeOptions {
        download_concurrency: cli.download_concurrency,
        cwebp: cli.cwebp.clone(),
        scratch_dir: scratch.clone(),
        retain_png_dir: Some(cli.output_dir.join("frames")),
    };
    tokio::fs::create_dir_all(&cli.output_dir).await?;

    let output = compose_and_report(
        &event,
        &source,
        &FsSink::new(cli.output_dir.clone(), cli.batch_index),
        &options,
    )
    .await?;

    // Emit a stable local summary for scripts and benchmarks.
    let summary = serde_json::json!({
        "job_id": output.job_id,
        "batch": output.batch,
        "batch_manifest_key": output.batch_manifest_key,
        "mode": output.mode,
        "output_dir": cli.output_dir.to_string_lossy(),
        "frames_dir": cli.output_dir.join("frames").to_string_lossy(),
    });
    let _ = tokio::fs::remove_dir_all(&scratch).await;
    Ok(summary)
}

fn scratch_dir() -> Result<PathBuf, ComposeError> {
    let dir = std::env::temp_dir().join(format!("aqw-rust-compose-scratch-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}
