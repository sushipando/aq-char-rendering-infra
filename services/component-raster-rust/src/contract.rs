//! Serde models for the RasterComponentState event, the prepare manifest
//! subset, and the result-record contract shared with the Python worker.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::import::AuthoredColorTransform;

/// The event produced by either `RasterComponentStates` Map mode.
///
/// ```json
/// {"job_id": "...", "manifest_key": "jobs/.../prepare/manifest.json", "task_index": 0}
/// ```
#[derive(Clone, Debug, Deserialize)]
pub struct RasterEvent {
    pub job_id: String,
    pub manifest_key: String,
    pub task_index: i64,
    #[serde(default, rename = "benchmark_output_prefix")]
    pub benchmark_output_prefix: Option<String>,
    // Optional per-invocation backend override (local-raster A/B testing).
    // Production map events omit it; the manifest is authoritative there.
    #[serde(default, rename = "raster_backend")]
    pub raster_backend: Option<String>,
}

/// The prepare-manifest fields the raster worker depends on.
#[derive(Clone, Debug, Deserialize)]
pub struct PrepareManifest {
    pub job_id: String,
    pub viewbox: Vec<f64>,
    pub settings: ManifestSettings,
    #[serde(default)]
    pub cache: ManifestCacheSettings,
    pub fields: HashMap<String, String>,
    #[serde(default)]
    pub all_color_rules: Vec<Vec<String>>,
    #[serde(default)]
    pub component_raster_space: Option<String>,
    #[serde(default)]
    pub component_tasks: Vec<ComponentTask>,
    #[serde(default)]
    pub parts: HashMap<String, Part>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ManifestCacheSettings {
    #[serde(default = "default_true")]
    pub components: bool,
}

impl Default for ManifestCacheSettings {
    fn default() -> Self {
        Self { components: true }
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct ManifestSettings {
    pub raster_size: i64,
    pub output_size: i64,
    pub zoom: f64,
    pub webp_quality: f64,
    pub webp_method: i64,
    // SVG rasterizer for the component pass: "resvg" (default, upstream
    // 0.48.1 + vendored patches) or "thorvg" (1.1.1). Manifests written by
    // older prepare versions omit it; default keeps them on resvg.
    #[serde(default = "default_raster_backend")]
    pub raster_backend: String,
}

fn default_raster_backend() -> String {
    "resvg".to_string()
}

fn default_true() -> bool {
    true
}

#[derive(Clone, Debug, Deserialize)]
pub struct ComponentTask {
    pub task_id: String,
    pub symbol_key: String,
    #[serde(default)]
    pub layer_name: String,
    #[serde(default)]
    pub layer_index: i64,
    #[serde(default)]
    pub matrix: Vec<f64>,
    #[serde(default)]
    pub darken: bool,
    /// New manifests point directly at the immutable, content-addressed SVG.
    /// Archive fields remain readable for executions already in flight.
    #[serde(default)]
    pub svg_key: Option<String>,
    #[serde(default)]
    pub bundle_key: String,
    #[serde(default)]
    pub member: String,
    #[serde(default)]
    pub state_signature: Option<String>,
    /// Probe registration-space bounds, validated against the prepared artwork
    /// before allocating a smaller raster. Absent on legacy manifests.
    #[serde(default)]
    pub raster_bounds: Option<RasterBounds>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RasterBounds {
    pub policy: String,
    pub bounds: Option<[f64; 4]>,
}

impl RasterBounds {
    pub fn validate(&self) -> Result<(), crate::error::RasterError> {
        if self.policy != crate::region::POLICY
            || self.bounds.is_some_and(|b| {
                !b.iter().all(|v| v.is_finite())
                    || b[2] <= 0.0
                    || b[3] <= 0.0
                    || !(b[0] + b[2]).is_finite()
                    || !(b[1] + b[3]).is_finite()
            })
        {
            return Err(crate::error::RasterError::invalid(
                "invalid component raster bounds",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct Part {
    #[serde(default)]
    pub hand_visibility: HashMap<String, String>,
    #[serde(default)]
    pub root_class: String,
    #[serde(default)]
    pub character_id: Option<i64>,
    #[serde(default)]
    pub color_rules: HashMap<String, Vec<String>>,
    #[serde(default)]
    pub placement_colors: HashMap<String, ColorTransformValues>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ColorTransformValues {
    #[serde(default = "default_mult")]
    pub red_mult: i64,
    #[serde(default = "default_mult")]
    pub green_mult: i64,
    #[serde(default = "default_mult")]
    pub blue_mult: i64,
    #[serde(default = "default_mult")]
    pub alpha_mult: i64,
    #[serde(default)]
    pub red_add: i64,
    #[serde(default)]
    pub green_add: i64,
    #[serde(default)]
    pub blue_add: i64,
    #[serde(default)]
    pub alpha_add: i64,
}

fn default_mult() -> i64 {
    256
}

impl From<ColorTransformValues> for AuthoredColorTransform {
    fn from(values: ColorTransformValues) -> Self {
        AuthoredColorTransform {
            red_mult: values.red_mult,
            green_mult: values.green_mult,
            blue_mult: values.blue_mult,
            alpha_mult: values.alpha_mult,
            red_add: values.red_add,
            green_add: values.green_add,
            blue_add: values.blue_add,
            alpha_add: values.alpha_add,
        }
    }
}

/// The complete result record written to S3 and returned to the Map. The
/// frame compositor consumes the same fields it reads today; unknown extra
/// fields are ignored by its schema.
#[derive(Clone, Debug, Serialize)]
pub struct RasterResult {
    pub task_id: String,
    pub empty: bool,
    pub x: i64,
    pub y: i64,
    pub width: i64,
    pub height: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
    pub png_key: Option<String>,
    pub input_bytes: u64,
    pub svg_bytes: u64,
    pub filter_count: usize,
    pub component_raster_space: String,
    pub canvas_width: i64,
    pub canvas_height: i64,
    pub state_signature: Option<String>,
    pub symbol_key: String,
    pub layer_name: String,
    pub result_key: String,
    // Which SVG rasterizer produced this raster (resvg | thorvg). Surface it
    // in the result record + telemetry so per-backend renders are auditable.
    pub render_backend: String,
    pub rasterize_ms: f64,
    pub crop_ms: f64,
    pub downsample_ms: f64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_map_event() {
        let raw = r#"{
            "job_id": "job-1",
            "manifest_key": "jobs/job-1/prepare/manifest.json",
            "task_index": 3,
            "benchmark_output_prefix": "benchmarks/rust-raster/job-1"
        }"#;
        let event: RasterEvent = serde_json::from_str(raw).unwrap();
        assert_eq!(event.task_index, 3);
        assert_eq!(
            event.benchmark_output_prefix.as_deref(),
            Some("benchmarks/rust-raster/job-1")
        );
        assert_eq!(event.raster_backend, None);
    }

    #[test]
    fn parses_manifest_raster_backend_with_default() {
        let raw = r#"{"job_id":"j","viewbox":[0.0,0.0,10.0,10.0],"settings":{"raster_size":512,"output_size":256,"zoom":1.0,"webp_quality":85.0,"webp_method":4},"fields":{},"component_tasks":[],"parts":{}}"#;
        let manifest: PrepareManifest = serde_json::from_str(raw).unwrap();
        assert_eq!(manifest.settings.raster_backend, "resvg");
        assert!(manifest.cache.components);

        let raw_thorvg = r#"{"job_id":"j","viewbox":[0.0,0.0,10.0,10.0],"settings":{"raster_size":512,"output_size":256,"zoom":1.0,"webp_quality":85.0,"webp_method":4,"raster_backend":"thorvg"},"fields":{},"component_tasks":[],"parts":{}}"#;
        let manifest: PrepareManifest = serde_json::from_str(raw_thorvg).unwrap();
        assert_eq!(manifest.settings.raster_backend, "thorvg");

        let raw_no_cache = r#"{"job_id":"j","viewbox":[0.0,0.0,10.0,10.0],"settings":{"raster_size":512,"output_size":256,"zoom":1.0,"webp_quality":85.0,"webp_method":4},"cache":{"components":false},"fields":{},"component_tasks":[],"parts":{}}"#;
        let manifest: PrepareManifest = serde_json::from_str(raw_no_cache).unwrap();
        assert!(!manifest.cache.components);
    }

    #[test]
    fn parses_placement_colors() {
        let raw = r#"{"job_id":"j","viewbox":[0.0,0.0,10.0,10.0],"settings":{"raster_size":512,"output_size":256,"zoom":1.0,"webp_quality":85.0,"webp_method":4},
            "fields":{},"all_color_rules":[["Base","dark"]],"component_raster_space":"output",
            "component_tasks":[],
            "parts":{"armor":{"root_class":"Armor","character_id":286,"color_rules":{},"placement_colors":{"286,11":{"red_mult":128,"alpha_add":7}}}}
        }"#;
        let manifest: PrepareManifest = serde_json::from_str(raw).unwrap();
        let part = manifest.parts.get("armor").unwrap();
        assert_eq!(part.root_class, "Armor");
        assert_eq!(part.character_id, Some(286));
        let transform: AuthoredColorTransform = part.placement_colors["286,11"].clone().into();
        assert_eq!(transform.red_mult, 128);
        assert_eq!(transform.alpha_mult, 256);
        assert_eq!(transform.alpha_add, 7);
    }

    #[test]
    fn rejects_missing_task_index() {
        let raw = r#"{"job_id":"j","manifest_key":"k"}"#;
        assert!(serde_json::from_str::<RasterEvent>(raw).is_err());
    }
}
