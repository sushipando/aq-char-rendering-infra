use std::collections::BTreeMap;

use anyhow::{ensure, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const VECTOR_SCHEMA: u32 = 6;
pub const FFDEC_VERSION: &str = "26.2.1";
// Bump whenever effective-export corrections or the pinned renderer change.
pub const EXPORT_POLICY: &str = "rust-effective-svg-v5-bank-pet-idle";
pub const BOUNDS_POLICY: &str = "resvg-0.48.1-aqw-v1-cells-v2-visibility";
// Final-container changes must not invalidate immutable intermediate caches.
pub const FINALIZE_POLICY: &str = "webp-adjacent-runs-v1";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ProbeConfig {
    pub policy: String,
    pub resolution: u32,
    pub retry_resolution: u32,
    pub padding_pixels: u32,
    pub zoom: f64,
}

impl ProbeConfig {
    pub fn new(zoom: f64) -> Self {
        Self {
            policy: BOUNDS_POLICY.into(),
            resolution: 256,
            retry_resolution: 1024,
            padding_pixels: 1,
            zoom,
        }
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(self.policy == BOUNDS_POLICY, "unsupported bounds policy");
        ensure!(
            (64..=1024).contains(&self.resolution),
            "invalid probe resolution"
        );
        ensure!(
            (self.resolution..=2048).contains(&self.retry_resolution),
            "invalid retry resolution"
        );
        ensure!(
            (1..=8).contains(&self.padding_pixels),
            "invalid probe padding"
        );
        ensure!(
            self.zoom.is_finite() && (0.25..=8.0).contains(&self.zoom),
            "invalid export zoom"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct StateRef {
    pub sha256: String,
    pub svg_key: String,
}

impl StateRef {
    pub fn new(bytes: &[u8]) -> Self {
        let sha256 = crate::sha256(bytes);
        Self {
            svg_key: format!("vector-svg/{VECTOR_SCHEMA}/{sha256}.svg"),
            sha256,
        }
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.sha256.len() == 64
                && self
                    .sha256
                    .bytes()
                    .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "invalid SVG digest"
        );
        ensure!(
            self.svg_key == format!("vector-svg/{VECTOR_SCHEMA}/{}.svg", self.sha256),
            "SVG key does not match digest"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SymbolRequest {
    pub key: String,
    pub class_name: String,
    pub character_id: u16,
    pub frame: usize,
    #[serde(default = "one")]
    pub root_timeline_frames: usize,
}

fn one() -> usize {
    1
}

fn enabled() -> bool {
    true
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SymbolExport {
    pub request: SymbolRequest,
    pub schedule: Vec<String>,
    pub mirror_flip_frame: usize,
    pub random_pose_as3: bool,
    pub animated_span: usize,
    pub settled_stop_frame: Option<usize>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SourceManifest {
    pub schema_version: u32,
    pub export_policy: String,
    pub source_sha256: String,
    pub export_identity: String,
    pub symbols: BTreeMap<String, SymbolExport>,
    pub states: BTreeMap<String, StateRef>,
    #[serde(default)]
    pub hand_visibility: BTreeMap<String, String>,
    pub color_rules: BTreeMap<String, Vec<String>>,
    pub placement_colors: BTreeMap<String, Value>,
    #[serde(default)]
    pub timeline_decisions: Vec<crate::timeline::Decision>,
}

impl SourceManifest {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema_version == VECTOR_SCHEMA && self.export_policy == EXPORT_POLICY,
            "incompatible vector cache"
        );
        ensure!(!self.symbols.is_empty(), "empty source manifest");
        for (hash, state) in &self.states {
            state.validate()?;
            ensure!(hash == &state.sha256, "state map digest mismatch");
        }
        for (key, symbol) in &self.symbols {
            ensure!(
                key == &symbol.request.key && !symbol.schedule.is_empty(),
                "invalid symbol schedule"
            );
            ensure!(
                symbol
                    .schedule
                    .iter()
                    .all(|hash| self.states.contains_key(hash)),
                "schedule references missing state"
            );
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProbeTask {
    pub state: StateRef,
    pub config: ProbeConfig,
    pub result_key: String,
    #[serde(default = "enabled")]
    pub cache_enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_id: Option<String>,
}

impl ProbeTask {
    pub fn new(state: StateRef, config: ProbeConfig) -> Result<Self> {
        state.validate()?;
        config.validate()?;
        let identity = crate::digest(&(&state.sha256, &config))?;
        Ok(Self {
            state,
            config,
            result_key: format!("svg-bounds/v1/{identity}.json"),
            cache_enabled: true,
            job_id: None,
        })
    }

    pub fn without_cache(state: StateRef, config: ProbeConfig, job_id: &str) -> Result<Self> {
        let parsed = uuid::Uuid::parse_str(job_id)?.to_string();
        ensure!(
            job_id.to_lowercase() == parsed,
            "invalid bounds task job id"
        );
        state.validate()?;
        config.validate()?;
        let identity = crate::digest(&(&state.sha256, &config))?;
        Ok(Self {
            state,
            config,
            result_key: format!("jobs/{job_id}/prepare/bounds-results/{identity}.json"),
            cache_enabled: false,
            job_id: Some(job_id.into()),
        })
    }

    pub fn validate(&self) -> Result<()> {
        self.state.validate()?;
        self.config.validate()?;
        let identity = crate::digest(&(&self.state.sha256, &self.config))?;
        let expected = if self.cache_enabled {
            ensure!(self.job_id.is_none(), "cached bounds task has a job scope");
            format!("svg-bounds/v1/{identity}.json")
        } else {
            let job_id = self
                .job_id
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("uncached bounds task is missing its job scope"))?;
            let parsed = uuid::Uuid::parse_str(job_id)?.to_string();
            ensure!(
                job_id.to_lowercase() == parsed,
                "invalid bounds task job id"
            );
            format!("jobs/{job_id}/prepare/bounds-results/{identity}.json")
        };
        ensure!(
            expected == self.result_key,
            "bounds result key does not match task identity"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Visibility {
    Visible,
    ConfirmedInvisible,
    Uncertain,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BoundsResult {
    pub schema_version: u32,
    pub state_sha256: String,
    pub config: ProbeConfig,
    pub visibility: Visibility,
    /// Registration-space x, y, width, height. Uncertain results MUST have
    /// a conservative bound; only proven invisible states may omit it.
    pub bounds: Option<[f64; 4]>,
    /// Registration-space declared page for later alpha-generating authored
    /// transforms that can invalidate a raw-SVG visibility/bounds observation.
    pub declared_bounds: Option<[f64; 4]>,
    pub resolution_used: u32,
    pub fallback_reason: Option<String>,
}

impl BoundsResult {
    pub fn validate(&self, task: &ProbeTask) -> Result<()> {
        task.validate()?;
        ensure!(
            self.schema_version == 1
                && self.state_sha256 == task.state.sha256
                && self.config == task.config,
            "bounds result identity mismatch"
        );
        ensure!(
            self.bounds.is_none() == (self.visibility == Visibility::ConfirmedInvisible),
            "unresolved bounds result"
        );
        if let Some(b) = self.bounds {
            ensure!(
                b.iter().all(|v| v.is_finite()) && b[2] > 0.0 && b[3] > 0.0,
                "invalid bounds geometry"
            );
        }
        if let Some(b) = self.declared_bounds {
            ensure!(
                b.iter().all(|v| v.is_finite()) && b[2] > 0.0 && b[3] > 0.0,
                "invalid declared bounds geometry"
            );
        }
        ensure!(
            self.resolution_used == self.config.resolution
                || self.resolution_used == self.config.retry_resolution
                || self.resolution_used == 0,
            "invalid bounds resolution"
        );
        ensure!(
            self.visibility != Visibility::Uncertain || self.fallback_reason.is_some(),
            "unexplained conservative bound"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BoundsPlan {
    pub schema_version: u32,
    pub job_id: String,
    pub input_key: String,
    pub source_manifests: BTreeMap<usize, String>,
    pub states: BTreeMap<String, ProbeTask>,
}
