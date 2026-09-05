//! Small in-memory probes. No PNG encoding, temporary image, or raster CLI.
use std::{
    collections::{BTreeMap, BTreeSet},
    time::Instant,
};

use anyhow::{ensure, Context, Result};
use aqw_component_raster::svg;
use futures::{stream, StreamExt, TryStreamExt};
use resvg::{tiny_skia, usvg};
use serde_json::json;

use crate::{
    model::*,
    store::{self, Store},
};

/// Per-job bounds fan-out mode. The request selects this explicitly; the
/// planner never changes modes based on the number of tasks.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BoundsMode {
    Inline,
    Distributed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PlanOptions {
    pub cache_enabled: bool,
    pub mode: BoundsMode,
}

impl PlanOptions {
    pub const fn new(cache_enabled: bool, mode: BoundsMode) -> Self {
        Self {
            cache_enabled,
            mode,
        }
    }
}

impl BoundsMode {
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "inline" => Ok(Self::Inline),
            "distributed" => Ok(Self::Distributed),
            _ => anyhow::bail!("invalid bounds_mode {value:?}"),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Inline => "inline",
            Self::Distributed => "distributed",
        }
    }
}

// Step Functions caps state input/output at 256 KiB. Leave room for the
// request, prepare result, and source-export results surrounding this field.
const INLINE_TASK_PAYLOAD_LIMIT_BYTES: usize = 160 * 1024;

fn dimension(value: Option<&str>) -> Result<f64> {
    let raw = value.context("SVG dimension missing")?.trim();
    let number: f64 = raw.strip_suffix("px").unwrap_or(raw).parse()?;
    ensure!(number.is_finite() && number >= 0.0, "invalid SVG dimension");
    Ok(number)
}

/// Probe pixel cells -> SVG page -> the original FFDec registration space.
/// A 1-pixel border is added BEFORE converting coordinate systems.
fn registration(
    cells: [f64; 4],
    viewport: [f64; 4],
    pixels: [u32; 2],
    wrapper: [f64; 6],
) -> [f64; 4] {
    let sx = viewport[2] / pixels[0] as f64;
    let sy = viewport[3] / pixels[1] as f64;
    [
        (viewport[0] + cells[0] * sx - wrapper[4]) / wrapper[0],
        (viewport[1] + cells[1] * sy - wrapper[5]) / wrapper[3],
        cells[2] * sx / wrapper[0],
        cells[3] * sy / wrapper[3],
    ]
}

struct Scan {
    cells: Option<[f64; 4]>,
    size: [u32; 2],
    max_alpha: u8,
    faint_outlier: bool,
}

fn scan(tree: &usvg::Tree, resolution: u32, padding: u32) -> Result<Scan> {
    let size = tree.size();
    let scale = resolution as f32 / size.width().max(size.height());
    let width = (size.width() * scale).ceil().max(1.0) as u32;
    let height = (size.height() * scale).ceil().max(1.0) as u32;
    let mut pixmap = tiny_skia::Pixmap::new(width, height).context("cannot allocate probe")?;
    // Match the actual integer thumbnail dimensions, including thin SVGs.
    resvg::render(
        tree,
        tiny_skia::Transform::from_scale(
            width as f32 / size.width(),
            height as f32 / size.height(),
        ),
        &mut pixmap.as_mut(),
    );
    let mut min_x = width;
    let mut min_y = height;
    let mut max_x = 0;
    let mut max_y = 0;
    let mut max_alpha = 0;
    let mut core = [width, height, 0, 0];
    for (index, pixel) in pixmap.data().as_chunks::<4>().0.iter().enumerate() {
        if pixel[3] == 0 {
            continue;
        }
        let x = index as u32 % width;
        let y = index as u32 / width;
        min_x = min_x.min(x);
        min_y = min_y.min(y);
        max_x = max_x.max(x);
        max_y = max_y.max(y);
        max_alpha = max_alpha.max(pixel[3]);
        if pixel[3] > 8 {
            core[0] = core[0].min(x);
            core[1] = core[1].min(y);
            core[2] = core[2].max(x);
            core[3] = core[3].max(y);
        }
    }
    let cells = (max_alpha > 0).then(|| {
        // Include whole occupied cells, NOT just their center or last index.
        let left = min_x as f64 - padding as f64;
        let top = min_y as f64 - padding as f64;
        let right = max_x as f64 + 1.0 + padding as f64;
        let bottom = max_y as f64 + 1.0 + padding as f64;
        [left, top, right - left, bottom - top]
    });
    Ok(Scan {
        cells,
        size: [width, height],
        max_alpha,
        // An isolated weak mark can determine an entire canvas edge even
        // when the main artwork is opaque. Validate it, not just all-faint
        // images. Ignore the normal one-cell antialiasing fringe.
        faint_outlier: max_alpha > 8
            && (min_x + 2 < core[0]
                || min_y + 2 < core[1]
                || max_x > core[2] + 2
                || max_y > core[3] + 2),
    })
}

pub fn union(a: [f64; 4], b: [f64; 4]) -> [f64; 4] {
    let x = a[0].min(b[0]);
    let y = a[1].min(b[1]);
    [
        x,
        y,
        (a[0] + a[2]).max(b[0] + b[2]) - x,
        (a[1] + a[3]).max(b[1] + b[3]) - y,
    ]
}

pub fn probe(bytes: &[u8], task: &ProbeTask) -> Result<BoundsResult> {
    task.validate()?;
    ensure!(
        crate::sha256(bytes) == task.state.sha256,
        "SVG checksum mismatch"
    );
    let document = svg::parse(bytes)?;
    ensure!(document.root.local() == "svg", "not an SVG document");
    let width = dimension(document.root.get("width"))?;
    let height = dimension(document.root.get("height"))?;
    let rendered: Vec<_> = document
        .root
        .children
        .iter()
        .filter(|n| !matches!(n.local(), "defs" | "metadata" | "title" | "desc" | "style"))
        .collect();
    let mut result = BoundsResult {
        schema_version: 1,
        state_sha256: task.state.sha256.clone(),
        config: task.config.clone(),
        visibility: Visibility::ConfirmedInvisible,
        bounds: None,
        resolution_used: 0,
        fallback_reason: None,
    };
    if width == 0.0 || height == 0.0 || rendered.is_empty() {
        result.validate(task)?;
        return Ok(result);
    }
    ensure!(rendered.len() == 1, "ambiguous FFDec registration wrapper");
    let wrapper = rendered[0]
        .get("transform")
        .and_then(svg::parse_matrix)
        .context("missing registration matrix")?;
    ensure!(
        wrapper.iter().all(|v| v.is_finite())
            && wrapper[0] > 0.0
            && (wrapper[0] - wrapper[3]).abs() < 1e-5
            && wrapper[1].abs() < 1e-8
            && wrapper[2].abs() < 1e-8
            && ((wrapper[0] - task.config.zoom).abs() < 1e-5 || (wrapper[0] - 1.0).abs() < 1e-5),
        "unsupported FFDec registration matrix"
    );
    // FFDec exports use a page-sized viewport. Reject non-equivalent viewBox
    // values rather than silently computing a wrong registration transform.
    let viewport = [0.0, 0.0, width, height];
    if let Some(raw) = document.root.get("viewBox") {
        let values: Vec<f64> = raw
            .split(|c: char| c.is_whitespace() || c == ',')
            .filter(|s| !s.is_empty())
            .map(str::parse)
            .collect::<std::result::Result<_, _>>()?;
        ensure!(
            values.len() == 4
                && values
                    .iter()
                    .zip(viewport)
                    .all(|(a, b)| a.is_finite() && (a - b).abs() < 1e-5),
            "unsupported FFDec viewBox"
        );
    }
    // usvg does not resolve arbitrary network resources; no external resource
    // directory is configured. Image data embedded by FFDec is supported.
    let tree = usvg::Tree::from_data(bytes, &usvg::Options::default())
        .context("resvg SVG parse failed")?;
    let initial = scan(&tree, task.config.resolution, task.config.padding_pixels)?;
    result.resolution_used = task.config.resolution;
    result.bounds = initial
        .cells
        .map(|cells| registration(cells, viewport, initial.size, wrapper));
    // Touching the viewport edge is normal for FFDec's cropped pages; a
    // larger probe of that SAME viewport cannot reveal pixels beyond it.
    // The occupied-cell padding already covers this case without a retry.
    if initial.cells.is_none() || initial.max_alpha <= 8 || initial.faint_outlier {
        let retry = scan(
            &tree,
            task.config.retry_resolution,
            task.config.padding_pixels,
        )?;
        result.resolution_used = task.config.retry_resolution;
        let retry_bounds = retry
            .cells
            .map(|cells| registration(cells, viewport, retry.size, wrapper));
        result.bounds = match (result.bounds, retry_bounds) {
            (Some(a), Some(b)) => Some(union(a, b)),
            (a, b) => a.or(b),
        };
        result.fallback_reason = Some(
            if initial.cells.is_none() {
                "empty_low_resolution"
            } else if initial.faint_outlier {
                "faint_outlier"
            } else {
                "faint_pixels"
            }
            .into(),
        );
    }
    if result.bounds.is_some() {
        result.visibility = Visibility::Visible;
    } else {
        // An empty thumbnail is NOT proof of invisibility. Tiny/faint paths
        // can disappear at both probe resolutions. Retain the declared page
        // conservatively, with the same low-resolution safety border.
        result.visibility = Visibility::Uncertain;
        result.bounds = Some(registration(
            [
                -(task.config.padding_pixels as f64),
                -(task.config.padding_pixels as f64),
                initial.size[0] as f64 + 2.0 * task.config.padding_pixels as f64,
                initial.size[1] as f64 + 2.0 * task.config.padding_pixels as f64,
            ],
            viewport,
            initial.size,
            wrapper,
        ));
        result.fallback_reason = Some("empty_retry_conservative_header".into());
    }
    result.validate(task)?;
    Ok(result)
}

pub async fn run_probe(store: &dyn Store, bucket: &str, task: &ProbeTask) -> Result<BoundsResult> {
    let started = Instant::now();
    task.validate()?;
    if task.cache_enabled {
        if let Some(cached) = store::cached::<BoundsResult>(store, bucket, &task.result_key).await?
        {
            cached.validate(task)?;
            crate::log(
                "bounds_probe",
                json!({"state_sha256":task.state.sha256,"cache_enabled":true,"cache_hit":true,"total_ms":started.elapsed().as_secs_f64()*1000.0}),
            );
            return Ok(cached);
        }
    }
    let bytes = store
        .get(bucket, &task.state.svg_key)
        .await?
        .context("missing probe SVG")?;
    let download_ms = started.elapsed().as_secs_f64() * 1000.0;
    let result = probe(&bytes, task)?;
    let probe_ms = started.elapsed().as_secs_f64() * 1000.0 - download_ms;
    store::write(store, bucket, &task.result_key, &result, task.cache_enabled).await?;
    crate::log(
        "bounds_probe",
        json!({"state_sha256":task.state.sha256,"cache_enabled":task.cache_enabled,"cache_hit":false,"download_ms":download_ms,"probe_ms":probe_ms,"total_ms":started.elapsed().as_secs_f64()*1000.0,"resolution":result.resolution_used,"visibility":result.visibility,"fallback_reason":result.fallback_reason}),
    );
    Ok(result)
}

/// Publish every task dataset to S3 and expose it using the request-selected mode.
pub async fn plan(
    store: &dyn Store,
    bucket: &str,
    job_id: &str,
    input_key: &str,
    source_keys: BTreeMap<usize, String>,
    config: ProbeConfig,
    options: PlanOptions,
) -> Result<serde_json::Value> {
    let started = Instant::now();
    config.validate()?;
    let PlanOptions {
        cache_enabled,
        mode,
    } = options;
    let prepared: serde_json::Value = store::read(store, bucket, input_key).await?;
    ensure!(
        prepared["job_id"] == job_id,
        "prepare input belongs to another job"
    );
    let expected: BTreeSet<usize> = prepared["sources"]
        .as_array()
        .context("missing source list")?
        .iter()
        .map(|v| {
            v["idx"]
                .as_u64()
                .map(|n| n as usize)
                .context("invalid source index")
        })
        .collect::<Result<_>>()?;
    ensure!(
        expected == source_keys.keys().copied().collect(),
        "missing or unexpected source export"
    );
    let mut states = BTreeMap::new();
    let mut exported_frames = 0;
    for (index, key) in &source_keys {
        let manifest: SourceManifest = store::read(store, bucket, key).await?;
        manifest.validate()?;
        let source = prepared["sources"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["idx"].as_u64() == Some(*index as u64))
            .unwrap();
        ensure!(
            source["sha256"] == manifest.source_sha256,
            "source export identity mismatch"
        );
        exported_frames += manifest
            .symbols
            .values()
            .map(|s| s.schedule.len())
            .sum::<usize>();
        for (hash, state) in manifest.states {
            let task = if cache_enabled {
                ProbeTask::new(state, config.clone())?
            } else {
                ProbeTask::without_cache(state, config.clone(), job_id)?
            };
            states.entry(hash).or_insert(task);
        }
    }
    let missing: Vec<_> = if cache_enabled {
        let checks: Vec<_> = stream::iter(states.values().cloned())
            .map(|task| async move {
                let cached = store::cached::<BoundsResult>(store, bucket, &task.result_key).await?;
                if let Some(result) = cached {
                    result.validate(&task)?;
                    Ok(None)
                } else {
                    Ok::<_, anyhow::Error>(Some(task))
                }
            })
            .buffered(16)
            .try_collect()
            .await?;
        checks.into_iter().flatten().collect()
    } else {
        states.values().cloned().collect()
    };
    let inline_tasks = match mode {
        BoundsMode::Inline => {
            let encoded_size = serde_json::to_vec(&missing)?.len();
            ensure!(
                encoded_size <= INLINE_TASK_PAYLOAD_LIMIT_BYTES,
                "inline bounds task payload is {encoded_size} bytes, above the safe {}-byte limit; resubmit with --bounds-mode distributed",
                INLINE_TASK_PAYLOAD_LIMIT_BYTES
            );
            serde_json::to_value(&missing)?
        }
        BoundsMode::Distributed => serde_json::Value::Null,
    };
    let plan_key = format!("jobs/{job_id}/prepare/bounds-plan.json");
    let tasks_key = format!("jobs/{job_id}/prepare/bounds-tasks.json");
    let plan = BoundsPlan {
        schema_version: 1,
        job_id: job_id.into(),
        input_key: input_key.into(),
        source_manifests: source_keys,
        states,
    };
    store::write(store, bucket, &tasks_key, &missing, false).await?;
    store::write(store, bucket, &plan_key, &plan, false).await?;
    crate::log(
        "plan_bounds",
        json!({"job_id":job_id,"cache_enabled":cache_enabled,"bounds_mode":mode.as_str(),"exported_frames":exported_frames,"unique_states":plan.states.len(),"cache_hits":if cache_enabled {plan.states.len()-missing.len()} else {0},"missing_states":missing.len(),"total_ms":started.elapsed().as_secs_f64()*1000.0}),
    );
    Ok(
        json!({"plan_key":plan_key,"tasks_key":tasks_key,"task_count":missing.len(),"bounds_mode":mode.as_str(),"inline_tasks":inline_tasks}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn svg(body: &str) -> Vec<u8> {
        format!(r#"<svg xmlns="http://www.w3.org/2000/svg" width="4096" height="2048"><g transform="matrix(2 0 0 2 200 100)">{body}</g></svg>"#).into_bytes()
    }
    fn task(bytes: &[u8]) -> ProbeTask {
        ProbeTask::new(StateRef::new(bytes), ProbeConfig::new(2.0)).unwrap()
    }

    #[test]
    fn occupied_cells_and_padding_map_through_registration() {
        let bytes = svg(r#"<rect x="100" y="100" width="400" height="200" fill="red"/>"#);
        let result = probe(&bytes, &task(&bytes)).unwrap();
        let b = result.bounds.unwrap();
        assert_eq!(result.visibility, Visibility::Visible);
        assert!(b[0] <= 100.0 && b[1] <= 100.0 && b[0] + b[2] >= 500.0 && b[1] + b[3] >= 300.0);
        assert!(b[2] < 430.0 && b[3] < 230.0);
        assert_eq!(
            registration(
                [10.0, 20.0, 2.0, 3.0],
                [0.0, 0.0, 4096.0, 2048.0],
                [256, 128],
                [2.0, 0.0, 0.0, 2.0, 200.0, 100.0]
            ),
            [-20.0, 110.0, 16.0, 24.0]
        );
    }

    #[test]
    fn empty_thumbnail_is_not_confirmed_invisible() {
        let bytes = svg(r#"<rect width="1" height="1" opacity="0"/>"#);
        let result = probe(&bytes, &task(&bytes)).unwrap();
        assert_eq!(result.visibility, Visibility::Uncertain);
        assert!(result.bounds.is_some());
        assert_eq!(result.resolution_used, 1024);
    }

    #[test]
    fn faint_outliers_are_validated_even_with_an_opaque_center() {
        let bytes = svg(
            r#"<rect x="100" y="100" width="100" height="100"/><rect x="900" y="600" width="80" height="80" opacity="0.02"/>"#,
        );
        let result = probe(&bytes, &task(&bytes)).unwrap();
        assert_eq!(result.resolution_used, 1024);
        assert_eq!(result.fallback_reason.as_deref(), Some("faint_outlier"));
        let b = result.bounds.unwrap();
        assert!(b[0] + b[2] >= 980.0 && b[1] + b[3] >= 680.0);
    }

    #[test]
    fn zero_page_is_confirmed_invisible_and_bad_svg_is_an_error() {
        let bytes = br#"<svg xmlns="http://www.w3.org/2000/svg" width="0" height="0"/>"#;
        assert_eq!(
            probe(bytes, &task(bytes)).unwrap().visibility,
            Visibility::ConfirmedInvisible
        );
        assert!(probe(b"bad SVG", &task(b"bad SVG")).is_err());
    }

    #[test]
    fn cache_keys_cover_bounds_policy_and_padding() {
        let bytes = svg("<path/>");
        let first = task(&bytes);
        let mut config = first.config.clone();
        config.padding_pixels = 2;
        let second = ProbeTask::new(first.state.clone(), config).unwrap();
        assert_ne!(first.result_key, second.result_key);
        let mut tampered = first;
        tampered.state.sha256 = "0".repeat(64);
        assert!(tampered.validate().is_err());
    }

    #[test]
    fn representative_padded_probes_enclose_full_resolution_alpha() {
        let cases = [
            r#"<path d="M10 10 L900 800" stroke="red" stroke-width="0.8"/>"#,
            r#"<g transform="matrix(-1 0 0 1 1000 0)"><rect x="120" y="130" width="400" height="250"/></g>"#,
            r#"<rect x="50" y="50" width="1200" height="400" opacity="0.015"/>"#,
            r#"<defs><filter id="glow" x="-50%" y="-50%" width="200%" height="200%"><feGaussianBlur stdDeviation="12"/></filter></defs><rect x="200" y="200" width="500" height="200" filter="url(#glow)"/>"#,
        ];
        for body in cases {
            for zoom in [1.0, 2.0] {
                let bytes = String::from_utf8(svg(body))
                    .unwrap()
                    .replace(
                        "matrix(2 0 0 2 200 100)",
                        &format!("matrix({zoom} 0 0 {zoom} 200 100)"),
                    )
                    .into_bytes();
                let tree = usvg::Tree::from_data(&bytes, &usvg::Options::default()).unwrap();
                let high = scan(&tree, 4096, 0).unwrap();
                let high = registration(
                    high.cells.unwrap(),
                    [0.0, 0.0, 4096.0, 2048.0],
                    high.size,
                    [zoom, 0.0, 0.0, zoom, 200.0, 100.0],
                );
                for padding in [1, 2] {
                    let mut config = ProbeConfig::new(zoom);
                    config.padding_pixels = padding;
                    let bounds = probe(
                        &bytes,
                        &ProbeTask::new(StateRef::new(&bytes), config).unwrap(),
                    )
                    .unwrap()
                    .bounds
                    .unwrap();
                    assert!(
                        bounds[0] <= high[0]
                            && bounds[1] <= high[1]
                            && bounds[0] + bounds[2] >= high[0] + high[2]
                            && bounds[1] + bounds[3] >= high[1] + high[3],
                        "{body}: {bounds:?} excludes {high:?}"
                    );
                }
            }
        }
    }
}
