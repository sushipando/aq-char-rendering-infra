use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use std::{collections::BTreeMap, path::PathBuf};

#[derive(Clone)]
pub struct Config {
    pub source_bucket: String,
    pub work_bucket: String,
    pub job_table: String,
    pub result_queue_url: String,
    pub bounds_queue_url: Option<String>,
    pub public_base_url: String,
    pub dataset_version: String,
    pub asset_manifest_key: String,
    pub renderer_version: String,
    pub ffdec: PathBuf,
    pub render_cache: bool,
    pub official_fallback: bool,
    pub component_frame_cap: usize,
    pub compose_batch_size: usize,
    pub frames_per_lambda: usize,
    pub download_concurrency: usize,
    pub bounds_resolution: u32,
    pub bounds_padding_pixels: u32,
    pub worker_concurrency: BTreeMap<String, i32>,
    pub defaults: Value,
}

pub fn env(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.into())
}
pub fn number(name: &str, default: usize, min: usize, max: usize) -> Result<usize> {
    let value = env(name, &default.to_string()).parse()?;
    ensure!((min..=max).contains(&value), "invalid configuration {name}");
    Ok(value)
}
pub fn boolean(name: &str, default: bool) -> Result<bool> {
    match env(name, if default { "true" } else { "false" })
        .to_lowercase()
        .as_str()
    {
        "true" | "1" | "yes" => Ok(true),
        "false" | "0" | "no" => Ok(false),
        _ => anyhow::bail!("invalid boolean {name}"),
    }
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let required = |key: &str| -> Result<String> {
            let value = std::env::var(key).with_context(|| format!("missing {key}"))?;
            ensure!(!value.trim().is_empty(), "empty {key}");
            Ok(value)
        };
        let dataset_version = required("CHAR_RENDER_ASSET_DATASET_VERSION")?;
        let defaults = json!({"raster_size":number("CHAR_RENDER_DEFAULT_RASTER_SIZE",2048,64,4096)?,"output_size":number("CHAR_RENDER_DEFAULT_OUTPUT_SIZE",2048,64,2048)?,"zoom":env("CHAR_RENDER_DEFAULT_ZOOM","2").parse::<f64>()?,"padding":number("CHAR_RENDER_DEFAULT_PADDING",0,0,1023)?,"complete_loop":boolean("CHAR_RENDER_DEFAULT_COMPLETE_LOOP",true)?,"max_frames":number("CHAR_RENDER_DEFAULT_MAX_FRAMES",360,1,2000)?,"subframe_start":number("CHAR_RENDER_DEFAULT_SUBFRAME_START",1,1,10000)?,"webp_quality":env("CHAR_RENDER_DEFAULT_WEBP_QUALITY","85").parse::<f64>()?,"webp_method":number("CHAR_RENDER_DEFAULT_WEBP_METHOD",4,0,6)?,"webp_lossless":false,"raster_backend":"resvg"});
        Ok(Self {
            source_bucket: required("CHAR_RENDER_SOURCE_BUCKET")?,
            work_bucket: required("CHAR_RENDER_WORK_BUCKET")?,
            job_table: required("CHAR_RENDER_JOB_TABLE")?,
            result_queue_url: required("CHAR_RENDER_RESULT_QUEUE_URL")?,
            bounds_queue_url: std::env::var("CHAR_RENDER_BOUNDS_QUEUE_URL")
                .ok()
                .filter(|value| !value.trim().is_empty()),
            public_base_url: required("CHAR_RENDER_PUBLIC_BASE_URL")?
                .trim_end_matches('/')
                .into(),
            asset_manifest_key: env(
                "CHAR_RENDER_ASSET_MANIFEST_KEY",
                &format!("datasets/{dataset_version}/manifest.json"),
            ),
            dataset_version,
            renderer_version: env("CHAR_RENDERER_VERSION", "v20-rust-bounds"),
            ffdec: env("CHAR_RENDER_FFDEC_PATH", "/opt/ffdec/ffdec-cli.jar").into(),
            render_cache: boolean("CHAR_RENDER_CACHE_ENABLED", true)?,
            official_fallback: boolean("CHAR_RENDER_ALLOW_OFFICIAL_ASSET_FALLBACK", true)?,
            component_frame_cap: number("CHAR_RENDER_COMPONENT_RASTER_FRAME_CAP", 25, 1, 2000)?,
            compose_batch_size: number(
                "CHAR_RENDER_COMPONENT_COMPOSE_FRAMES_PER_LAMBDA",
                1,
                1,
                2000,
            )?,
            frames_per_lambda: number("CHAR_RENDER_FRAMES_PER_LAMBDA", 30, 1, 2000)?,
            download_concurrency: number("CHAR_RENDER_FINALIZER_DOWNLOAD_CONCURRENCY", 16, 1, 128)?,
            bounds_resolution: number("CHAR_RENDER_BOUNDS_RESOLUTION", 256, 64, 1024)? as u32,
            bounds_padding_pixels: number("CHAR_RENDER_BOUNDS_PADDING_PIXELS", 1, 1, 8)? as u32,
            worker_concurrency: serde_json::from_str(&env("CHAR_RENDER_WORKER_CONCURRENCY", "{}"))?,
            defaults,
        })
    }
}
