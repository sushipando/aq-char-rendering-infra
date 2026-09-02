//! aqw-component-raster binary: Lambda runtime with a `local-raster` CLI for
//! parity fixtures.

use std::sync::OnceLock;
use std::time::Instant;

use aqw_component_raster::contract::{RasterEvent, RasterResult};
use aqw_component_raster::error::{LambdaError, RasterError};
use aqw_component_raster::local;
use aqw_component_raster::storage::S3Store;
use aqw_component_raster::telemetry;
use aqw_component_raster::worker::run_raster_task;
use aws_sdk_s3::Client as S3Client;
use lambda_runtime::{service_fn, LambdaEvent};
use tokio::sync::OnceCell;

static CLIENT: OnceCell<S3Client> = OnceCell::const_new();
static COLD_START: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);
static MODULE_LOADED_AT: OnceLock<Instant> = OnceLock::new();

struct LambdaConfig {
    work_bucket: String,
}

impl LambdaConfig {
    fn from_env() -> Result<LambdaConfig, RasterError> {
        let work_bucket = std::env::var("CHAR_RENDER_WORK_BUCKET")
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                RasterError::invalid(
                    "Missing required environment variable CHAR_RENDER_WORK_BUCKET",
                )
            })?;
        Ok(LambdaConfig { work_bucket })
    }
}

async fn lambda_handler(event: LambdaEvent<RasterEvent>) -> Result<RasterResult, LambdaError> {
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

    let (payload, _context) = event.into_parts();
    let result = run_raster_task(&payload, &store, &store)
        .await
        .map_err(Box::new)?;
    telemetry::log_raster_complete(
        &payload.job_id,
        payload.task_index,
        cold_start,
        module_age_ms,
        started.elapsed().as_secs_f64() * 1000.0,
    );
    Ok(result)
}

#[tokio::main]
async fn main() -> Result<(), LambdaError> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() > 1 {
        let cli = local::parse_cli(&args[1..]).map_err(Box::new)?;
        let _ = local::run_local(&cli).await.map_err(Box::new)?;
        return Ok(());
    }
    let function = service_fn(lambda_handler);
    lambda_runtime::run(function).await?;
    Ok(())
}
