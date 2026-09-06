//! Content hashing and the structured `component_compose_*` JSON log lines.

use sha2::{Digest, Sha256};

pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

/// Round an already-converted millisecond value to one decimal place like
/// Python's `round(v, 1)`.
pub fn rounded_ms(milliseconds: f64) -> f64 {
    (milliseconds * 10.0).round() / 10.0
}

/// Emit one JSON log line on stderr, matching the Python worker's
/// `log_event` output shape so the benchmark harness can parse CloudWatch.
pub fn log_json(fields: serde_json::Map<String, serde_json::Value>) {
    let payload = serde_json::Value::Object(fields);
    eprintln!("{}", payload);
}

use crate::contract::BatchIndex;

/// All per-phase timings collected for one chunk.
#[derive(Clone, Copy, Debug, Default)]
pub struct Timings {
    pub manifest_ms: f64,
    pub results_read_ms: f64,
    pub png_download_ms: f64,
    pub decode_ms: f64,
    pub composite_ms: f64,
    pub downsample_ms: f64,
    pub encode_ms: f64,
    pub upload_ms: f64,
    /// Upload time hidden behind a subsequent frame's WebP encode.
    pub upload_overlap_ms: f64,
    pub manifest_write_ms: f64,
}

impl Timings {
    pub fn accounted_ms(&self) -> f64 {
        self.manifest_ms
            + self.results_read_ms
            + self.png_download_ms
            + self.decode_ms
            + self.composite_ms
            + self.downsample_ms
            + self.encode_ms
            + self.upload_ms
            - self.upload_overlap_ms
            + self.manifest_write_ms
    }
}

/// Everything the profile and completion log events need.
#[derive(Clone, Debug)]
pub struct ChunkStats {
    pub job_id: String,
    pub batch: i64,
    pub frame_start: i64,
    pub frame_end: i64,
    /// Number of expensive composition/WebP encode operations performed.
    pub frames_rendered: i64,
    pub unique_frames_encoded: i64,
    pub logical_frames_emitted: i64,
    pub deduplicated_frames: i64,
    pub referenced_component_count: usize,
    pub downloaded_png_count: usize,
    pub png_bytes: u64,
    pub canvas_width: i64,
    pub canvas_height: i64,
    pub raster_size: i64,
    pub output_size: i64,
    pub component_raster_space: String,
    pub downsampled_in_compose: bool,
    pub download_concurrency: usize,
    pub timings: Timings,
    pub total_ms: f64,
}

fn field(
    fields: &mut serde_json::Map<String, serde_json::Value>,
    key: &str,
    value: impl Into<serde_json::Value>,
) {
    fields.insert(key.to_string(), value.into());
}

pub fn log_profile(stats: &ChunkStats) {
    let mut fields = serde_json::Map::new();
    field(&mut fields, "event", "component_compose_profile");
    field(&mut fields, "job_id", stats.job_id.clone());
    field(&mut fields, "batch", stats.batch);
    field(&mut fields, "frame_start", stats.frame_start);
    field(&mut fields, "frame_end", stats.frame_end);
    field(&mut fields, "frames_rendered", stats.frames_rendered);
    field(
        &mut fields,
        "unique_frames_encoded",
        stats.unique_frames_encoded,
    );
    field(
        &mut fields,
        "logical_frames_emitted",
        stats.logical_frames_emitted,
    );
    field(
        &mut fields,
        "deduplicated_frames",
        stats.deduplicated_frames,
    );
    field(
        &mut fields,
        "referenced_component_count",
        stats.referenced_component_count,
    );
    field(
        &mut fields,
        "downloaded_png_count",
        stats.downloaded_png_count,
    );
    field(&mut fields, "png_bytes", stats.png_bytes);
    field(&mut fields, "canvas_width", stats.canvas_width);
    field(&mut fields, "canvas_height", stats.canvas_height);
    field(&mut fields, "raster_size", stats.raster_size);
    field(&mut fields, "output_size", stats.output_size);
    field(
        &mut fields,
        "component_raster_space",
        stats.component_raster_space.clone(),
    );
    field(
        &mut fields,
        "downsampled_in_compose",
        stats.downsampled_in_compose,
    );
    field(&mut fields, "compositor", "rust");
    field(
        &mut fields,
        "download_concurrency",
        stats.download_concurrency,
    );
    let timings = stats.timings;
    field(&mut fields, "manifest_ms", rounded_ms(timings.manifest_ms));
    field(
        &mut fields,
        "results_read_ms",
        rounded_ms(timings.results_read_ms),
    );
    field(
        &mut fields,
        "png_download_ms",
        rounded_ms(timings.png_download_ms),
    );
    field(&mut fields, "decode_ms", rounded_ms(timings.decode_ms));
    field(
        &mut fields,
        "composite_ms",
        rounded_ms(timings.composite_ms),
    );
    field(
        &mut fields,
        "downsample_ms",
        rounded_ms(timings.downsample_ms),
    );
    field(&mut fields, "encode_ms", rounded_ms(timings.encode_ms));
    field(&mut fields, "upload_ms", rounded_ms(timings.upload_ms));
    field(
        &mut fields,
        "upload_overlap_ms",
        rounded_ms(timings.upload_overlap_ms),
    );
    field(
        &mut fields,
        "manifest_write_ms",
        rounded_ms(timings.manifest_write_ms),
    );
    field(&mut fields, "total_ms", rounded_ms(stats.total_ms));
    let unaccounted = stats.total_ms - timings.accounted_ms();
    field(&mut fields, "unaccounted_ms", rounded_ms(unaccounted));
    field(
        &mut fields,
        "ms_per_frame",
        rounded_ms(stats.total_ms / stats.frames_rendered.max(1) as f64),
    );
    field(
        &mut fields,
        "ms_per_logical_frame",
        rounded_ms(stats.total_ms / stats.logical_frames_emitted.max(1) as f64),
    );
    log_json(fields);
}

pub fn log_complete(
    job_id: &str,
    batch: i64,
    batch_index: &BatchIndex,
    compositor: &str,
    cold_start: bool,
    module_age_ms: f64,
    duration_ms: f64,
) {
    let mut fields = serde_json::Map::new();
    field(&mut fields, "event", "component_compose_complete");
    field(&mut fields, "job_id", job_id.to_string());
    field(&mut fields, "batch", batch);
    if let Some(frame_start) = batch_index.frame_start {
        field(&mut fields, "frame_start", frame_start);
    }
    if let Some(frame_end) = batch_index.frame_end {
        field(&mut fields, "frame_end", frame_end);
    }
    if let Some(composition_start) = batch_index.composition_start {
        field(&mut fields, "composition_start", composition_start);
    }
    if let Some(composition_end) = batch_index.composition_end {
        field(&mut fields, "composition_end", composition_end);
    }
    field(&mut fields, "compositor", compositor.to_string());
    field(&mut fields, "cold_start", cold_start);
    field(&mut fields, "module_age_ms", rounded_ms(module_age_ms));
    field(&mut fields, "duration_ms", rounded_ms(duration_ms));
    log_json(fields);
}
