//! Serde models for the Step Functions event, the prepare manifest, and the
//! compose-batch output contract. All shapes are shared with the Python
//! worker so CDK can switch the Lambda implementation without touching
//! `FinalizeAnimation`.

use serde::{Deserialize, Serialize};

/// The Lambda event produced by `ComposeComponentFrameChunks`.
///
/// ```json
/// {
///   "job_id": "...",
///   "manifest_key": "jobs/.../prepare/manifest.json",
///   "component_results_key": "jobs/.../component/manifest.json",
///   "batch": {"index": 0, "composition_start": 0, "composition_end": 0}
/// }
/// ```
///
/// `benchmark_output_prefix` is a benchmark-only field; when present every
/// composed frame and the batch manifest land under that S3 prefix so a
/// candidate deployment never touches a completed job's intermediate keys.
#[derive(Clone, Debug, Deserialize)]
pub struct ComposeEvent {
    pub job_id: String,
    pub manifest_key: String,
    #[serde(default)]
    pub component_results: Vec<ComponentResult>,
    /// Production uses S3 instead of carrying all raster metadata in Map state.
    /// Inline records remain supported for local benchmarks and old executions.
    #[serde(default)]
    pub component_results_key: Option<String>,
    pub batch: BatchIndex,
    #[serde(default, rename = "benchmark_output_prefix")]
    pub benchmark_output_prefix: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct BatchIndex {
    pub index: i64,
    #[serde(default)]
    pub frame_start: Option<i64>,
    #[serde(default)]
    pub frame_end: Option<i64>,
    #[serde(default)]
    pub composition_start: Option<usize>,
    #[serde(default)]
    pub composition_end: Option<usize>,
}

/// One compact component workflow result (the same fields
/// `component_workflow_result` keeps out of Step Functions state).
#[derive(Clone, Debug, Deserialize)]
pub struct ComponentResult {
    pub task_id: String,
    #[serde(default)]
    pub empty: bool,
    #[serde(default)]
    pub png_key: Option<String>,
    #[serde(default)]
    pub sha256: Option<String>,
    #[serde(default)]
    pub x: i64,
    #[serde(default)]
    pub y: i64,
    #[serde(default)]
    pub component_raster_space: Option<String>,
}

/// The fields of the prepare manifest the composer depends on. Extra fields
/// (parts, fields, color rules, batches, ...) are deliberately ignored.
#[derive(Clone, Debug, Deserialize)]
pub struct PrepareManifest {
    #[serde(default)]
    pub presentation_layers: Option<PresentationLayers>,
    pub job_id: String,
    pub frame_count: i64,
    pub viewbox: Vec<f64>,
    pub frame_durations: Vec<i64>,
    pub settings: ManifestSettings,
    #[serde(default)]
    pub component_pipeline: Option<bool>,
    #[serde(default)]
    pub component_raster_space: Option<String>,
    pub component_frames: Vec<ComponentFrame>,
    #[serde(default)]
    pub component_compositions: Vec<ComponentComposition>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct StaticLayer {
    pub key: String,
    pub sha256: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct PresentationLayers {
    /// Composited after the first (background) component, before the character.
    #[serde(default)]
    pub background_overlay: Option<StaticLayer>,
    #[serde(default)]
    pub background: Option<StaticLayer>,
    #[serde(default)]
    pub foreground: Option<StaticLayer>,
}

fn default_output_format() -> String { "webp".into() }

#[derive(Clone, Debug, Deserialize)]
pub struct ManifestSettings {
    #[serde(default = "default_output_format")]
    pub output_format: String,
    #[serde(default)]
    pub rgba_compression: Option<String>,
    pub raster_size: i64,
    pub output_size: i64,
    pub webp_quality: f64,
    pub webp_method: i64,
    #[serde(default)]
    pub webp_lossless: Option<bool>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ComponentFrame {
    pub number: i64,
    pub layers: Vec<String>,
    #[serde(default)]
    pub duration_ms: Option<i64>,
}

/// One exact full-frame pixel recipe and every logical animation frame that
/// uses it. `canonical_frame` owns the single encoded WebP object.
#[derive(Clone, Debug, Deserialize)]
pub struct ComponentComposition {
    pub canonical_frame: i64,
    pub layers: Vec<String>,
    pub logical_frames: Vec<i64>,
}

/// One record in the compose-batch manifest; the finalizer consumes
/// `webp_key`, `canvas_width`, `canvas_height`, `duration`, `sha256`, and
/// `bytes` exactly as it does today.
#[derive(Clone, Debug, Serialize)]
pub struct FrameRecord {
    pub frame: i64,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub webp_key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rgba_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rgba_compression: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_sha256: Option<String>,
    pub x: i64,
    pub y: i64,
    pub width: i64,
    pub height: i64,
    pub canvas_width: i64,
    pub canvas_height: i64,
    pub duration: i64,
    pub sha256: String,
    pub bytes: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct BatchManifest {
    pub schema_version: i64,
    pub job_id: String,
    pub batch: i64,
    pub frames: Vec<FrameRecord>,
    pub warnings: Vec<String>,
}

/// The result returned to Step Functions (`payloadResponseOnly`).
#[derive(Clone, Debug, Serialize)]
pub struct ComposeOutput {
    pub job_id: String,
    pub batch: i64,
    pub batch_manifest_key: String,
    pub mode: String,
}

impl ComposeOutput {
    pub fn component_raster(job_id: String, batch: i64, batch_manifest_key: String) -> Self {
        ComposeOutput {
            job_id,
            batch,
            batch_manifest_key,
            mode: "component-raster".to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_step_functions_event() {
        let raw = r#"{
            "job_id": "job-1",
            "manifest_key": "jobs/job-1/prepare/manifest.json",
            "component_results": [
                {"task_id": "aaa", "empty": false, "png_key": "jobs/job-1/component/rasters/aaa.png", "sha256": "abc", "x": 3, "y": 4, "component_raster_space": "output"},
                {"task_id": "bbb", "empty": true}
            ],
            "batch": {"index": 0, "frame_start": 1, "frame_end": 10},
            "compositor": "pillow"
        }"#;
        let event: ComposeEvent = serde_json::from_str(raw).unwrap();
        assert_eq!(event.job_id, "job-1");
        assert_eq!(event.batch.index, 0);
        assert_eq!(event.batch.frame_start, Some(1));
        assert_eq!(event.batch.frame_end, Some(10));
        assert_eq!(event.batch.composition_start, None);
        assert_eq!(event.component_results.len(), 2);
        assert!(event.component_results[1].empty);
        assert!(event.component_results[1].png_key.is_none());
        assert!(event.benchmark_output_prefix.is_none());
    }

    #[test]
    fn parses_a_unique_composition_batch() {
        let raw = r#"{
            "job_id": "job-1",
            "manifest_key": "jobs/job-1/prepare/manifest.json",
            "component_results": [],
            "batch": {"index": 3, "composition_start": 7, "composition_end": 7}
        }"#;
        let event: ComposeEvent = serde_json::from_str(raw).unwrap();
        assert_eq!(event.batch.frame_start, None);
        assert_eq!(event.batch.composition_start, Some(7));
        assert_eq!(event.batch.composition_end, Some(7));
    }

    #[test]
    fn parses_benchmark_output_prefix() {
        let raw = r#"{
            "job_id": "job-1",
            "manifest_key": "jobs/job-1/prepare/manifest.json",
            "component_results": [],
            "batch": {"index": 0, "frame_start": 1, "frame_end": 10},
            "benchmark_output_prefix": "benchmarks/rust-compose/job-1"
        }"#;
        let event: ComposeEvent = serde_json::from_str(raw).unwrap();
        assert_eq!(
            event.benchmark_output_prefix.as_deref(),
            Some("benchmarks/rust-compose/job-1")
        );
    }

    #[test]
    fn frame_record_serializes_with_preserved_schema() {
        let record = FrameRecord {
            frame: 1,
            rgba_key: None,
            rgba_compression: None,
            raw_sha256: None,
            webp_key: "jobs/job-1/component/webp-frames/000001.webp".to_string(),
            x: 0,
            y: 0,
            width: 256,
            height: 256,
            canvas_width: 256,
            canvas_height: 256,
            duration: 42,
            sha256: "deadbeef".to_string(),
            bytes: 1024,
        };
        let value: serde_json::Value = serde_json::to_value(record).unwrap();
        assert_eq!(value["frame"], 1);
        assert_eq!(value["width"], 256);
        assert_eq!(value["duration"], 42);
        assert_eq!(value["sha256"], "deadbeef");
        assert_eq!(value["canvas_width"], 256);
    }

    #[test]
    fn batch_manifest_uses_schema_version_one() {
        let manifest = BatchManifest {
            schema_version: 1,
            job_id: "job-1".to_string(),
            batch: 3,
            frames: vec![],
            warnings: vec![],
        };
        let value = serde_json::to_value(manifest).unwrap();
        assert_eq!(value["schema_version"], 1);
        assert_eq!(value["batch"], 3);
        assert_eq!(value["warnings"], serde_json::json!([]));
    }

    #[test]
    fn output_contract_matches_python() {
        let output = ComposeOutput::component_raster(
            "job-1".to_string(),
            0,
            "jobs/job-1/component/compose-batches/batch-0000.json".to_string(),
        );
        let value = serde_json::to_value(output).unwrap();
        assert_eq!(value["mode"], "component-raster");
        assert_eq!(value["batch"], 0);
        assert_eq!(value["job_id"], "job-1");
    }

    #[test]
    fn rejects_missing_batch() {
        let raw = r#"{"job_id": "job-1", "manifest_key": "k"}"#;
        assert!(serde_json::from_str::<ComposeEvent>(raw).is_err());
    }
}
