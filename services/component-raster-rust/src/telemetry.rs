//! Structured `component_raster_profile` logging compatible with the Python
//! worker, plus hashing helpers.

use sha2::{Digest, Sha256};

pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

pub fn rounded(value: f64) -> f64 {
    (value * 10.0).round() / 10.0
}

pub fn rounded2(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

#[derive(Clone, Copy, Debug, Default)]
pub struct RasterTimings {
    pub manifest_ms: f64,
    pub bundle_download_ms: f64,
    pub extract_ms: f64,
    pub import_ms: f64,
    pub svg_build_ms: f64,
    pub rasterize_ms: f64,
    pub crop_ms: f64,
    pub downsample_ms: f64,
    pub upload_ms: f64,
}

impl RasterTimings {
    pub fn accounted_ms(&self) -> f64 {
        self.manifest_ms
            + self.bundle_download_ms
            + self.extract_ms
            + self.import_ms
            + self.svg_build_ms
            + self.rasterize_ms
            + self.crop_ms
            + self.downsample_ms
            + self.upload_ms
    }
}

#[derive(Clone, Debug)]
pub struct RasterStats {
    pub job_id: String,
    pub task_id: String,
    pub symbol_key: String,
    pub layer_name: String,
    pub empty: bool,
    pub svg_bytes: u64,
    pub input_bytes: u64,
    pub filter_count: usize,
    pub raster_pixel_count: u64,
    pub component_raster_space: String,
    pub raster_backend: crate::raster::RenderBackend,
    pub raster_canvas: (i64, i64),
    pub output_canvas: (i64, i64),
    pub cache_enabled: bool,
    pub cache_hit: bool,
    pub timings: RasterTimings,
    pub total_ms: f64,
}

fn field(
    fields: &mut serde_json::Map<String, serde_json::Value>,
    key: &str,
    value: impl Into<serde_json::Value>,
) {
    fields.insert(key.to_string(), value.into());
}

pub fn log_raster_profile(stats: &RasterStats) {
    let mut fields = serde_json::Map::new();
    field(&mut fields, "event", "component_raster_profile");
    field(&mut fields, "job_id", stats.job_id.clone());
    field(&mut fields, "task_id", stats.task_id.clone());
    field(&mut fields, "symbol_key", stats.symbol_key.clone());
    field(&mut fields, "layer_name", stats.layer_name.clone());
    field(&mut fields, "empty", stats.empty);
    field(&mut fields, "svg_bytes", stats.svg_bytes);
    field(&mut fields, "input_bytes", stats.input_bytes);
    field(&mut fields, "filter_count", stats.filter_count);
    field(&mut fields, "raster_pixel_count", stats.raster_pixel_count);
    field(
        &mut fields,
        "component_raster_space",
        stats.component_raster_space.clone(),
    );
    field(&mut fields, "render_backend", stats.raster_backend.to_str());
    field(&mut fields, "raster_canvas_width", stats.raster_canvas.0);
    field(&mut fields, "raster_canvas_height", stats.raster_canvas.1);
    field(&mut fields, "output_canvas_width", stats.output_canvas.0);
    field(&mut fields, "output_canvas_height", stats.output_canvas.1);
    field(&mut fields, "cache_enabled", stats.cache_enabled);
    field(&mut fields, "cache_hit", stats.cache_hit);
    let timings = stats.timings;
    field(&mut fields, "manifest_ms", rounded(timings.manifest_ms));
    field(
        &mut fields,
        "bundle_download_ms",
        rounded(timings.bundle_download_ms),
    );
    field(&mut fields, "extract_ms", rounded(timings.extract_ms));
    field(&mut fields, "import_ms", rounded(timings.import_ms));
    field(&mut fields, "svg_build_ms", rounded(timings.svg_build_ms));
    field(&mut fields, "rasterize_ms", rounded(timings.rasterize_ms));
    field(&mut fields, "crop_ms", rounded(timings.crop_ms));
    field(&mut fields, "downsample_ms", rounded(timings.downsample_ms));
    field(&mut fields, "upload_ms", rounded(timings.upload_ms));
    field(&mut fields, "total_ms", rounded(stats.total_ms));
    field(
        &mut fields,
        "unaccounted_ms",
        rounded(stats.total_ms - timings.accounted_ms()),
    );
    eprintln!("{}", serde_json::Value::Object(fields));
}

pub fn log_raster_complete(
    job_id: &str,
    task_index: i64,
    cold_start: bool,
    module_age_ms: f64,
    duration_ms: f64,
) {
    let mut fields = serde_json::Map::new();
    field(&mut fields, "event", "component_raster_complete");
    field(&mut fields, "job_id", job_id.to_string());
    field(&mut fields, "task_index", task_index);
    field(&mut fields, "cold_start", cold_start);
    field(&mut fields, "module_age_ms", rounded(module_age_ms));
    field(&mut fields, "duration_ms", rounded(duration_ms));
    eprintln!("{}", serde_json::Value::Object(fields));
}
