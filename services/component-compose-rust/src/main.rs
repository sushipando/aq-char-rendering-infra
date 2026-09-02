//! aqw-component-compose: native Rust component-compose Lambda + local mode.
//!
//! Under Lambda (`AWS_LAMBDA_RUNTIME_API` set, no CLI arguments) this runs the
//! Rust Lambda runtime and serves the existing `ComposeComponentFrameChunk`
//! contract. With a `local-compose ...` argument it runs one whole chunk from
//! the downloaded production artifact layout without mocking S3.

use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Instant;

use aqw_component_compose::contract::{ComposeEvent, ComposeOutput};
use aqw_component_compose::error::{ComposeError, LambdaError};
use aqw_component_compose::local;
use aqw_component_compose::storage::S3Store;
use aqw_component_compose::telemetry;
use aqw_component_compose::worker::{compose_and_report, ComposeOptions};
use aws_sdk_s3::Client as S3Client;
use lambda_runtime::{service_fn, LambdaEvent};
use tokio::sync::OnceCell;

static CLIENT: OnceCell<S3Client> = OnceCell::const_new();
static COLD_START: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);
static MODULE_LOADED_AT: OnceLock<Instant> = OnceLock::new();

struct LambdaConfig {
    work_bucket: String,
    download_concurrency: usize,
    cwebp: PathBuf,
    scratch_dir: PathBuf,
}

impl LambdaConfig {
    fn from_env() -> Result<LambdaConfig, ComposeError> {
        let work_bucket = required_env("CHAR_RENDER_WORK_BUCKET")?;
        let download_concurrency = env_value("CHAR_RENDER_COMPONENT_COMPOSE_DOWNLOAD_CONCURRENCY")
            .or_else(|| env_value("CHAR_RENDER_FINALIZER_DOWNLOAD_CONCURRENCY"))
            .map(|value| value.parse::<usize>().unwrap_or(32))
            .unwrap_or(32)
            .clamp(1, 256);
        let cwebp = env_value("CHAR_RENDER_CWEBP")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/opt/libwebp/bin/cwebp"));
        let scratch_dir = env_value("CHAR_RENDER_TEMP_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/tmp"));
        let _ = std::fs::create_dir_all(&scratch_dir);
        Ok(LambdaConfig {
            work_bucket,
            download_concurrency,
            cwebp,
            scratch_dir,
        })
    }
}

fn required_env(name: &str) -> Result<String, ComposeError> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            ComposeError::invalid(format!("Missing required environment variable {name}"))
        })
}

fn env_value(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

async fn lambda_handler(event: LambdaEvent<ComposeEvent>) -> Result<ComposeOutput, LambdaError> {
    let started = Instant::now();
    let cold_start = COLD_START.swap(false, std::sync::atomic::Ordering::SeqCst);
    let module_age_ms = {
        let loaded = MODULE_LOADED_AT.get_or_init(Instant::now);
        loaded.elapsed().as_secs_f64() * 1000.0
    };

    let config = LambdaConfig::from_env().map_err(Box::new)?;
    let client = CLIENT
        .get_or_init(|| async {
            let shared = aws_config::load_from_env().await;
            S3Client::new(&shared)
        })
        .await;
    let store = S3Store::new(client.clone(), config.work_bucket.clone());
    let options = ComposeOptions {
        download_concurrency: config.download_concurrency,
        cwebp: config.cwebp.clone(),
        scratch_dir: config.scratch_dir.clone(),
        retain_png_dir: None,
    };

    let (payload, _context) = event.into_parts();
    let result = compose_and_report(&payload, &store, &store, &options)
        .await
        .map_err(Box::new)?;

    telemetry::log_complete(
        &payload.job_id,
        result.batch,
        &payload.batch,
        "rust",
        cold_start,
        module_age_ms,
        (started.elapsed().as_secs_f64()) * 1000.0,
    );
    Ok(result)
}

#[tokio::main]
async fn main() -> Result<(), LambdaError> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() > 1 {
        let cli = local::parse_cli(&args[1..])?;
        let summary = local::run_local(&cli).await?;
        println!("{}", serde_json::to_string_pretty(&summary)?);
        return Ok(());
    }
    let function = service_fn(lambda_handler);
    lambda_runtime::run(function).await
}
