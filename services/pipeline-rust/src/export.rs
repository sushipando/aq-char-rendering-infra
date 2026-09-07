//! The exporter never rasterizes an image. It can publish bounds-prefetch
//! tasks after each unique SVG is durably stored.
use anyhow::{ensure, Context, Result};
use aqw_component_raster::{
    import::IDENTITY,
    svg::{self, FFDEC_NS, XLINK_NS},
};
use futures::{stream, StreamExt, TryStreamExt};
use notify::{
    event::{AccessKind, AccessMode, CreateKind, ModifyKind, RenameMode},
    Event, EventKind, RecursiveMode, Watcher,
};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Mutex,
    },
    time::{Duration, Instant},
};
use tokio::process::Command;

use crate::{
    model::*,
    queue::BoundsPublisher,
    store::{self, Store},
    swf::Swf,
};

pub struct Ffdec {
    pub jar: PathBuf,
    pub deadline: Instant,
}

#[derive(Clone, Copy)]
struct FrameRange {
    start: usize,
    count: usize,
}

impl Ffdec {
    pub async fn run(&self, home: &Path, args: &[String]) -> Result<()> {
        tokio::fs::create_dir_all(home).await?;
        let remaining = self
            .deadline
            .checked_duration_since(Instant::now())
            .context("FFDec deadline exhausted")?;
        let output = tokio::time::timeout(
            remaining,
            Command::new("java")
                .arg(format!("-Duser.home={}", home.display()))
                .arg("-Djava.awt.headless=true")
                .arg("-jar")
                .arg(&self.jar)
                .args(args)
                .kill_on_drop(true)
                .output(),
        )
        .await
        .context("FFDec timed out (child terminated)")??;
        ensure!(
            output.status.success(),
            "FFDec failed: {}",
            String::from_utf8_lossy(&output.stderr[output.stderr.len().saturating_sub(2000)..])
        );
        Ok(())
    }

    async fn frames(
        &self,
        source: &Path,
        requests: &[SymbolRequest],
        destination: &Path,
        zoom: f64,
        range: FrameRange,
        live_prefetch: Option<&SvgPrefetcher<'_>>,
    ) -> Result<BTreeMap<String, Vec<Vec<u8>>>> {
        let FrameRange { start, count } = range;
        ensure!(
            start > 0 && count > 0 && count <= 2008,
            "invalid export frame range"
        );
        let mut result = BTreeMap::new();
        for nested in [true, false] {
            let selected: Vec<_> = requests
                .iter()
                .filter(|r| (r.root_timeline_frames == 1) == nested)
                .collect();
            if selected.is_empty() {
                continue;
            }
            ensure!(
                selected
                    .iter()
                    .all(|r| r.frame > 0 && r.root_timeline_frames > 0),
                "invalid symbol timeline"
            );
            let output = destination.join(if nested { "nested" } else { "root" });
            let schedules: BTreeMap<_, Vec<_>> = selected
                .iter()
                .map(|r| {
                    (
                        r.key.clone(),
                        (0..count)
                            .map(|i| {
                                if nested {
                                    start + i
                                } else {
                                    r.frame + (start - 1 + i) % r.root_timeline_frames
                                }
                            })
                            .collect(),
                    )
                })
                .collect();
            let mut args = vec!["-zoom".into(), zoom.to_string()];
            if nested && (count > 1 || start > 1) {
                args.extend(["-sublength".into(), (start + count - 1).to_string()]);
            }
            args.extend([
                "-selectid".into(),
                selected
                    .iter()
                    .map(|r| r.character_id.to_string())
                    .collect::<Vec<_>>()
                    .join(","),
                "-select".into(),
                selected
                    .iter()
                    .map(|r| {
                        format!(
                            "{}:{}",
                            r.character_id,
                            if nested {
                                r.frame.to_string()
                            } else {
                                ranges(&schedules[&r.key])
                            }
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(","),
                "-format".into(),
                "sprite:svg".into(),
                "-export".into(),
                "sprite".into(),
                output.to_string_lossy().into_owned(),
                source.to_string_lossy().into_owned(),
            ]);
            if let Some(prefetcher) = live_prefetch {
                self.run_streaming(&destination.join("ffdec-home"), &args, &output, prefetcher)
                    .await?;
            } else {
                self.run(&destination.join("ffdec-home"), &args).await?;
            }
            for request in selected {
                let prefix = format!("DefineSprite_{}", request.character_id);
                let mut directories = Vec::new();
                for entry in std::fs::read_dir(&output)? {
                    let entry = entry?;
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if entry.file_type()?.is_dir()
                        && (name == prefix || name.starts_with(&format!("{prefix}_")))
                    {
                        directories.push(entry.path());
                    }
                }
                directories.sort();
                let mut frames = Vec::new();
                // Read each distinct physical SVG only once (root schedules
                // can wrap many times without making FFDec export repeats).
                let mut loaded = BTreeMap::new();
                for frame in &schedules[&request.key] {
                    if !loaded.contains_key(frame) {
                        let mut candidates: Vec<_> = directories
                            .iter()
                            .map(|d| {
                                if nested {
                                    d.join(request.frame.to_string())
                                        .join(format!("{frame}.svg"))
                                } else {
                                    d.join(format!("{frame}.svg"))
                                }
                            })
                            .collect();
                        if nested && count == 1 && start == 1 {
                            candidates.extend(
                                directories
                                    .iter()
                                    .map(|d| d.join(format!("{}.svg", request.frame))),
                            );
                        }
                        let path =
                            candidates
                                .into_iter()
                                .find(|p| p.is_file())
                                .with_context(|| {
                                    format!("FFDec omitted {} frame {frame}", request.key)
                                })?;
                        loaded.insert(*frame, tokio::fs::read(path).await?);
                    }
                    frames.push(loaded[frame].clone());
                }
                result.insert(request.key.clone(), frames);
            }
        }
        Ok(result)
    }

    async fn run_streaming(
        &self,
        home: &Path,
        args: &[String],
        output_root: &Path,
        prefetcher: &SvgPrefetcher<'_>,
    ) -> Result<()> {
        tokio::fs::create_dir_all(home).await?;
        tokio::fs::create_dir_all(output_root).await?;
        let (event_sender, mut events) = tokio::sync::mpsc::unbounded_channel();
        let mut watcher = match notify::recommended_watcher(move |event| {
            let _ = event_sender.send(event);
        }) {
            Ok(watcher) => watcher,
            Err(error) => {
                crate::log(
                    "ffdec_svg_watch_unavailable",
                    json!({"output_root":output_root.to_string_lossy(),"error":error.to_string()}),
                );
                return self.run(home, args).await;
            }
        };
        if let Err(error) = watcher.watch(output_root, RecursiveMode::Recursive) {
            crate::log(
                "ffdec_svg_watch_unavailable",
                json!({"output_root":output_root.to_string_lossy(),"error":error.to_string()}),
            );
            return self.run(home, args).await;
        }
        let remaining = self
            .deadline
            .checked_duration_since(Instant::now())
            .context("FFDec deadline exhausted")?;
        let command = Command::new("java")
            .arg(format!("-Duser.home={}", home.display()))
            .arg("-Djava.awt.headless=true")
            .arg("-jar")
            .arg(&self.jar)
            .args(args)
            .kill_on_drop(true)
            .output();
        let run = async {
            tokio::pin!(command);
            let mut observed = BTreeSet::new();
            let mut streamed = 0usize;
            let process_output = loop {
                tokio::select! {
                    result = &mut command => break result?,
                    Some(event) = events.recv() => {
                        let mut scan = match event {
                            Ok(event) => svg_completion_event(&event),
                            Err(error) => {
                                crate::log(
                                    "ffdec_svg_watch_error",
                                    json!({"output_root":output_root.to_string_lossy(),"error":error.to_string()}),
                                );
                                false
                            }
                        };
                        // Coalesce events that arrived while the previous S3/SQS
                        // submissions were in flight, then scan once.
                        while let Ok(event) = events.try_recv() {
                            match event {
                                Ok(event) => scan |= svg_completion_event(&event),
                                Err(error) => crate::log(
                                    "ffdec_svg_watch_error",
                                    json!({"output_root":output_root.to_string_lossy(),"error":error.to_string()}),
                                ),
                            }
                        }
                        if scan {
                            streamed += stream_completed_svgs(output_root, &mut observed, prefetcher).await;
                        }
                    }
                }
            };
            // Reconcile once after exit because filesystem events are an
            // optimization, not the source of truth. The later manifest pass
            // also validates the final normalized export set.
            streamed += stream_completed_svgs(output_root, &mut observed, prefetcher).await;
            crate::log(
                "ffdec_svg_stream_complete",
                json!({
                    "output_root": output_root.to_string_lossy(),
                    "completed_files_observed": observed.len(),
                    "unique_prefetches_started": streamed,
                }),
            );
            ensure!(
                process_output.status.success(),
                "FFDec failed: {}",
                String::from_utf8_lossy(
                    &process_output.stderr[process_output.stderr.len().saturating_sub(2000)..]
                )
            );
            Ok(())
        };
        let result = tokio::time::timeout(remaining, run)
            .await
            .context("FFDec timed out (child terminated)")?;
        drop(watcher);
        result
    }
}

fn svg_completion_event(event: &Event) -> bool {
    event.need_rescan()
        || (event
            .paths
            .iter()
            .any(|path| path.extension().is_some_and(|extension| extension == "svg"))
            && matches!(
                event.kind,
                EventKind::Access(AccessKind::Close(AccessMode::Write | AccessMode::Any))
                    | EventKind::Create(CreateKind::File | CreateKind::Any)
                    | EventKind::Modify(ModifyKind::Data(_))
                    | EventKind::Modify(ModifyKind::Name(
                        RenameMode::To | RenameMode::Both | RenameMode::Any
                    ))
                    | EventKind::Any
            ))
}

fn svg_files(root: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    let mut pending = vec![root.to_owned()];
    while let Some(directory) = pending.pop() {
        for entry in std::fs::read_dir(directory)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            if kind.is_dir() {
                pending.push(entry.path());
            } else if kind.is_file() && entry.path().extension().is_some_and(|e| e == "svg") {
                files.push(entry.path());
            }
        }
    }
    files.sort();
    Ok(files)
}

async fn stream_completed_svgs(
    root: &Path,
    observed: &mut BTreeSet<PathBuf>,
    prefetcher: &SvgPrefetcher<'_>,
) -> usize {
    let paths = match svg_files(root) {
        Ok(paths) => paths,
        Err(error) => {
            crate::log(
                "ffdec_svg_stream_scan_failed",
                json!({"output_root":root.to_string_lossy(),"error":error.to_string()}),
            );
            return 0;
        }
    };
    let ready = stream::iter(paths.into_iter().filter(|path| !observed.contains(path)))
        .map(|path| async move {
            let bytes = tokio::fs::read(&path).await.ok()?;
            // FFDec creates the destination before it finishes writing. Only
            // parseable documents are safe to expose to another Lambda.
            svg::parse(&bytes).ok()?;
            Some((path, bytes))
        })
        .buffer_unordered(16)
        .filter_map(async move |item| item)
        .collect::<Vec<_>>()
        .await;
    let submissions = ready.into_iter().map(|(path, bytes)| {
        // A complete SVG is immutable in FFDec's export layout. Marking the
        // path here avoids re-reading it after later filesystem events; the
        // manifest pass retries failed uploads/publications using final bytes.
        observed.insert(path.clone());
        async move { (path, prefetcher.submit(bytes).await) }
    });
    let mut started = 0;
    stream::iter(submissions)
        .buffer_unordered(16)
        .map(|(path, result)| match result {
            Ok(true) => started += 1,
            Ok(false) => {}
            Err(error) => crate::log(
                "bounds_prefetch_stream_failed",
                json!({"path":path.to_string_lossy(),"error":error.to_string()}),
            ),
        })
        .count()
        .await;
    started
}

fn ranges(values: &[usize]) -> String {
    let mut ordered = values.to_vec();
    ordered.sort_unstable();
    ordered.dedup();
    let mut result = Vec::new();
    let mut index = 0;
    while index < ordered.len() {
        let start = ordered[index];
        let mut end = start;
        index += 1;
        while index < ordered.len() && ordered[index] == end + 1 {
            end = ordered[index];
            index += 1;
        }
        result.push(if start == end {
            start.to_string()
        } else {
            format!("{start}-{end}")
        });
    }
    result.join(",")
}

#[derive(Default, Serialize, Deserialize)]
pub struct ScriptMetadata {
    #[serde(default)]
    pub hand_visibility: BTreeMap<String, String>,
    pub color_rules: BTreeMap<String, Vec<String>>,
    pub timelines: BTreeMap<String, crate::script::Class>,
    pub random_pose: bool,
}

impl ScriptMetadata {
    pub fn inspect(&mut self, text: &str) -> Result<()> {
        let folded = text.to_lowercase();
        self.random_pose |= Regex::new(r"(?is)gotoAndStop\s*\([^)]*Math\.random[^)]*\)")?
            .is_match(text)
            || (folded.contains("random")
                && folded.contains("gotoandstop")
                && folded.contains("totalframes"));
        let Some((name, timeline)) = crate::script::parse(text)? else {
            return Ok(());
        };
        if let Some(hand) = &timeline.hidden_in_hand { self.hand_visibility.insert(name.clone(), hand.clone()); }
        ensure!(self.timelines.insert(name.clone(), timeline).is_none(), "duplicate decompiled class {name}");
        let color = Regex::new(
            r#"(?:mcSetColor|setColor)\s*\(\s*this\s*,\s*['"]([^'"]+)['"]\s*,\s*['"]([^'"]+)['"]\s*\)"#,
        )?;
        if let Some(color) = color.captures(text) {
            self.color_rules
                .insert(name.clone(), vec![color[1].into(), color[2].into()]);
        }
        Ok(())
    }
}

fn script_files(root: &Path) -> Result<Vec<PathBuf>> {
    let mut result = Vec::new();
    if !root.is_dir() {
        return Ok(result);
    }
    let mut pending = vec![root.to_owned()];
    while let Some(directory) = pending.pop() {
        for entry in std::fs::read_dir(directory)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                pending.push(entry.path());
            } else if entry.path().extension().is_some_and(|e| e == "as") {
                result.push(entry.path());
            }
        }
    }
    result.sort();
    Ok(result)
}

fn mirror_flip(frames: &[Vec<u8>]) -> Result<usize> {
    if frames.len() < 16 {
        return Ok(0);
    }
    let fingerprint = |bytes: &[u8]| -> Result<Vec<(String, bool)>> {
        let document = svg::parse(bytes)?;
        let mut result = Vec::new();
        for node in document.root.iter().filter(|n| n.local() == "use") {
            let key = node
                .get_in("characterId", FFDEC_NS)
                .or_else(|| node.get_in("href", XLINK_NS))
                .or_else(|| node.get("href"))
                .unwrap_or("");
            if key.is_empty() {
                continue;
            }
            let m = node
                .get("transform")
                .and_then(svg::parse_matrix)
                .unwrap_or(IDENTITY);
            result.push((key.into(), m[0] * m[3] - m[1] * m[2] < 0.0));
        }
        result.sort();
        Ok(result)
    };
    let first = fingerprint(&frames[0])?;
    let mut run = 0;
    for (index, bytes) in frames.iter().enumerate().skip(1) {
        let current = fingerprint(bytes)?;
        if first.iter().map(|e| &e.0).eq(current.iter().map(|e| &e.0)) && first != current {
            run += 1;
            if run == 5 {
                return Ok(index + 1 - run);
            }
        } else {
            run = 0;
        }
    }
    Ok(0)
}

#[derive(Clone, Copy)]
pub struct BoundsPrefetch<'a> {
    pub publisher: &'a dyn BoundsPublisher,
    pub resolution: u32,
    pub padding_pixels: u32,
}

pub struct ExportOptions<'a> {
    pub jar: PathBuf,
    pub timeout: Duration,
    pub bounds_prefetch: Option<BoundsPrefetch<'a>>,
}

impl ExportOptions<'static> {
    pub fn without_prefetch(jar: PathBuf, timeout: Duration) -> Self {
        Self {
            jar,
            timeout,
            bounds_prefetch: None,
        }
    }
}

#[derive(Clone, Copy, Default)]
struct PrefetchStats {
    published: usize,
    failed: usize,
}

struct SvgPrefetcher<'a> {
    store: &'a dyn Store,
    bucket: &'a str,
    publisher: &'a dyn BoundsPublisher,
    config: ProbeConfig,
    cache_enabled: bool,
    job_id: &'a str,
    // A hash is retained only after its queue publication succeeds. Failed
    // streaming attempts can therefore be retried by the manifest pass.
    published_states: Mutex<BTreeSet<String>>,
    published: AtomicUsize,
    failed: AtomicUsize,
}

impl<'a> SvgPrefetcher<'a> {
    fn new(
        store: &'a dyn Store,
        bucket: &'a str,
        prefetch: BoundsPrefetch<'a>,
        config: ProbeConfig,
        cache_enabled: bool,
        job_id: &'a str,
    ) -> Self {
        Self {
            store,
            bucket,
            publisher: prefetch.publisher,
            config,
            cache_enabled,
            job_id,
            published_states: Mutex::new(BTreeSet::new()),
            published: AtomicUsize::new(0),
            failed: AtomicUsize::new(0),
        }
    }

    async fn submit(&self, bytes: Vec<u8>) -> Result<bool> {
        let state = StateRef::new(&bytes);
        {
            let mut published = self.published_states.lock().unwrap();
            if !published.insert(state.sha256.clone()) {
                return Ok(false);
            }
        }
        if let Err(error) = self
            .store
            .put(self.bucket, &state.svg_key, bytes, "image/svg+xml", true)
            .await
        {
            self.published_states.lock().unwrap().remove(&state.sha256);
            return Err(error);
        }
        let task = prefetch_task(
            state.clone(),
            self.config.clone(),
            self.cache_enabled,
            self.job_id,
        )?;
        let stats = publish_prefetch(self.publisher, task).await;
        self.published.fetch_add(stats.published, Ordering::Relaxed);
        self.failed.fetch_add(stats.failed, Ordering::Relaxed);
        if stats.failed > 0 {
            self.published_states.lock().unwrap().remove(&state.sha256);
        }
        Ok(stats.published > 0)
    }

    fn stats(&self) -> PrefetchStats {
        PrefetchStats {
            published: self.published.load(Ordering::Relaxed),
            failed: self.failed.load(Ordering::Relaxed),
        }
    }
}

fn prefetch_config(prefetch: BoundsPrefetch<'_>, zoom: f64) -> Result<ProbeConfig> {
    let mut config = ProbeConfig::new(zoom);
    config.resolution = prefetch.resolution;
    config.padding_pixels = prefetch.padding_pixels;
    config.validate()?;
    Ok(config)
}

fn prefetch_task(
    state: StateRef,
    config: ProbeConfig,
    cache_enabled: bool,
    job_id: &str,
) -> Result<ProbeTask> {
    if cache_enabled {
        ProbeTask::new(state, config)
    } else {
        ProbeTask::without_cache(state, config, job_id)
    }
}

async fn publish_prefetch(publisher: &dyn BoundsPublisher, task: ProbeTask) -> PrefetchStats {
    match publisher.publish(&task).await {
        Ok(()) => PrefetchStats {
            published: 1,
            failed: 0,
        },
        Err(error) => {
            // Prefetch is latency optimization only. PlanBounds will detect
            // the missing result and run it through the selected barrier.
            crate::log(
                "bounds_prefetch_publish_failed",
                json!({"state_sha256":task.state.sha256,"error":error.to_string()}),
            );
            PrefetchStats {
                published: 0,
                failed: 1,
            }
        }
    }
}

async fn upload_and_prefetch(
    store: &dyn Store,
    bucket: &str,
    uploads: BTreeMap<String, Vec<u8>>,
    prefetcher: Option<&SvgPrefetcher<'_>>,
) -> Result<()> {
    stream::iter(uploads)
        .map(|(key, bytes)| async move {
            ensure!(
                StateRef::new(&bytes).svg_key == key,
                "SVG upload key does not match its contents"
            );
            if let Some(prefetcher) = prefetcher {
                prefetcher.submit(bytes).await?;
            } else {
                store
                    .put(bucket, &key, bytes, "image/svg+xml", true)
                    .await?;
            }
            Ok::<_, anyhow::Error>(())
        })
        .buffer_unordered(16)
        .try_collect::<Vec<()>>()
        .await?;
    Ok(())
}

pub async fn export_source(
    store: &dyn Store,
    work_bucket: &str,
    source_bucket: &str,
    event: &Value,
    options: ExportOptions<'_>,
) -> Result<Value> {
    let started = Instant::now();
    let ExportOptions {
        jar,
        timeout,
        bounds_prefetch,
    } = options;
    let job = event["job_id"].as_str().context("missing job id")?;
    let input_key = event["input_key"].as_str().context("missing input key")?;
    let prepared: Value = store::read(store, work_bucket, input_key).await?;
    ensure!(
        prepared["job_id"] == job,
        "prepare input belongs to another job"
    );
    let source = &event["source"];
    ensure!(
        prepared["sources"]
            .as_array()
            .context("missing sources")?
            .iter()
            .any(|s| s["idx"] == source["idx"]
                && s["sha256"] == source["sha256"]
                && s["key"] == source["key"]
                && s["requests"] == source["requests"]
                && s.get("normalization_requests") == source.get("normalization_requests")),
        "source does not match prepare input"
    );
    let source_hash = source["sha256"]
        .as_str()
        .context("missing source checksum")?;
    let source_idx = source["idx"].as_u64().context("invalid source index")?;
    let vector_cache = prepared["cache"]["vectors"].as_bool().unwrap_or(true);
    let bounds_cache = prepared["cache"]["bounds"].as_bool().unwrap_or(true);
    let requests: Vec<SymbolRequest> = serde_json::from_value(source["requests"].clone())?;
    let normalization_requests: Vec<SymbolRequest> = serde_json::from_value(
        source.get("normalization_requests").unwrap_or(&source["requests"]).clone(),
    )?;
    ensure!(!requests.is_empty(), "empty export unit");
    let settings = &prepared["settings"];
    let zoom = settings["zoom"].as_f64().context("missing zoom")?;
    let probe_config = bounds_prefetch
        .map(|prefetch| prefetch_config(prefetch, zoom))
        .transpose()?;
    let prefetcher = bounds_prefetch
        .zip(probe_config.clone())
        .map(|(prefetch, config)| {
            SvgPrefetcher::new(store, work_bucket, prefetch, config, bounds_cache, job)
        });
    let start = settings["subframe_start"]
        .as_u64()
        .context("missing subframe start")? as usize;
    let count = prepared["export_frame_count"]
        .as_u64()
        .context("missing export count")? as usize;
    let mut export_inputs = json!({"schema":VECTOR_SCHEMA,"policy":EXPORT_POLICY,"ffdec":FFDEC_VERSION,"sha256":source_hash,"requests":requests,"zoom":zoom,"start":start,"count":count});
    if source.get("normalization_requests").is_some() {
        export_inputs["normalization_requests"] = serde_json::to_value(&normalization_requests)?;
    }
    let identity = crate::digest(&export_inputs)?;
    let manifest_key = if vector_cache {
        format!("vector-manifests/{VECTOR_SCHEMA}/{identity}.json")
    } else {
        format!("jobs/{job}/prepare/vector-manifests/{source_idx}-{identity}.json")
    };
    if vector_cache {
        if let Some(cached) =
            store::cached::<SourceManifest>(store, work_bucket, &manifest_key).await?
        {
            cached.validate()?;
            ensure!(
                cached.export_identity == identity && cached.source_sha256 == source_hash,
                "vector cache identity mismatch"
            );
            let presence: Vec<bool> = stream::iter(cached.states.values())
                .map(|state| store.exists(work_bucket, &state.svg_key))
                .buffered(16)
                .try_collect()
                .await?;
            if presence.iter().all(|b| *b) {
                crate::log(
                    "prepare_export_complete",
                    json!({"job_id":job,"source_idx":source_idx,"cache_enabled":true,"cache_hit":true,"unique_states":cached.states.len(),"bounds_prefetch_published":0,"bounds_prefetch_failed":0,"duration_ms":started.elapsed().as_secs_f64()*1000.0}),
                );
                return Ok(
                    json!({"job_id":job,"source_idx":source_idx,"manifest_key":manifest_key,"vector_cache_hit":true}),
                );
            }
        }
    }
    let temporary = tempfile::tempdir()?;
    let bytes = store
        .get(
            source_bucket,
            source["key"].as_str().context("missing source key")?,
        )
        .await?
        .context("missing source SWF")?;
    ensure!(
        crate::sha256(&bytes) == source_hash,
        "source SWF checksum mismatch"
    );
    let swf = Swf::parse(&bytes)?;
    let source_path = temporary.path().join("source.swf");
    tokio::fs::write(&source_path, &bytes).await?;
    let ffdec = Ffdec {
        jar,
        deadline: started + timeout,
    };
    let metadata_key =
        format!("source-metadata/{EXPORT_POLICY}/{FFDEC_VERSION}/{source_hash}.json");
    let cached_metadata = if vector_cache {
        store::cached::<ScriptMetadata>(store, work_bucket, &metadata_key).await?
    } else {
        None
    };
    let metadata = if let Some(cached) = cached_metadata {
        cached
    } else {
        let output = temporary.path().join("scripts");
        ffdec
            .run(
                &temporary.path().join("script-home"),
                &[
                    "-onerror".into(),
                    "ignore".into(),
                    "-export".into(),
                    "script".into(),
                    output.to_string_lossy().into_owned(),
                    source_path.to_string_lossy().into_owned(),
                ],
            )
            .await?;
        let mut computed = ScriptMetadata::default();
        for path in script_files(&output)? {
            computed.inspect(&String::from_utf8_lossy(&std::fs::read(&path)?)).with_context(|| format!("decompiled script {}", path.display()))?;
        }
        if vector_cache {
            store::write(store, work_bucket, &metadata_key, &computed, true).await?;
        }
        computed
    };
    let normalized = crate::timeline::normalize(&bytes, &swf, &metadata.timelines, &normalization_requests)?;
    let selected: BTreeSet<_> = requests.iter().map(|r| r.key.as_str()).collect();
    let effective_requests: Vec<_> = normalized.requests.iter()
        .filter(|r| selected.contains(r.key.as_str())).cloned().collect();
    ensure!(effective_requests.len() == requests.len(), "export unit is missing from normalization context");
    tokio::fs::write(&source_path, &normalized.bytes).await?;
    crate::log("export_timeline_resolution", json!({"job_id":job,"source_idx":source_idx,"decisions":normalized.decisions}));
    let metadata_ms = started.elapsed().as_secs_f64() * 1000.0;
    let mut exported = ffdec
        .frames(
            &source_path,
            &effective_requests,
            &temporary.path().join("exports"),
            zoom,
            FrameRange { start, count },
            prefetcher.as_ref(),
        )
        .await?;
    let ffdec_ms = started.elapsed().as_secs_f64() * 1000.0 - metadata_ms;
    let mut manifest = SourceManifest {
        schema_version: VECTOR_SCHEMA,
        export_policy: EXPORT_POLICY.into(),
        source_sha256: source_hash.into(),
        export_identity: identity,
        symbols: BTreeMap::new(),
        states: BTreeMap::new(),
        hand_visibility: metadata.hand_visibility,
        color_rules: metadata.color_rules,
        placement_colors: swf.placement_colors()?,
        timeline_decisions: normalized.decisions.clone(),
    };
    let mut uploads = BTreeMap::new();
    for request in requests {
        let frames = exported
            .remove(&request.key)
            .context("missing requested export")?;
        let special = matches!(request.key.as_str(), "ground" | "pet");
        let flip = if special { mirror_flip(&frames)? } else { 0 };
        let random = special && metadata.random_pose;
        let mut schedule = Vec::new();
        for bytes in frames {
            let state = StateRef::new(&bytes);
            schedule.push(state.sha256.clone());
            uploads.entry(state.svg_key.clone()).or_insert(bytes);
            manifest.states.insert(state.sha256.clone(), state);
        }
        let effective = normalized.requests.iter().find(|r| r.key == request.key).context("missing normalized export request")?.clone();
        let stop = normalized.decisions.iter().find(|d| d.character_id == request.character_id).and_then(|d| match d.selection {
            crate::timeline::Selection::Hold { frame } => Some(frame),
            _ => None,
        });
        manifest.symbols.insert(
            request.key.clone(),
            SymbolExport {
                request: effective,
                schedule,
                mirror_flip_frame: flip,
                random_pose_as3: random,
                animated_span: if flip >= 2 {
                    flip
                } else if random {
                    1
                } else {
                    0
                },
                settled_stop_frame: stop,
            },
        );
    }
    manifest.validate()?;
    upload_and_prefetch(store, work_bucket, uploads, prefetcher.as_ref()).await?;
    let prefetch_stats = prefetcher
        .as_ref()
        .map(SvgPrefetcher::stats)
        .unwrap_or_default();
    // Publish LAST: readers never observe a manifest whose SVGs aren't there.
    store::write(store, work_bucket, &manifest_key, &manifest, vector_cache).await?;
    crate::log(
        "prepare_export_complete",
        json!({"job_id":job,"source_idx":source_idx,"cache_enabled":vector_cache,"cache_hit":false,"exported_frames":manifest.symbols.values().map(|s|s.schedule.len()).sum::<usize>(),"unique_states":manifest.states.len(),"bounds_prefetch_published":prefetch_stats.published,"bounds_prefetch_failed":prefetch_stats.failed,"metadata_ms":metadata_ms,"ffdec_ms":ffdec_ms,"duration_ms":started.elapsed().as_secs_f64()*1000.0}),
    );
    Ok(
        json!({"job_id":job,"source_idx":source_idx,"manifest_key":manifest_key,"vector_cache_hit":false}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::FsStore;
    use std::sync::Mutex;

    struct FakePublisher<'a> {
        store: &'a FsStore,
        seen: Mutex<Vec<ProbeTask>>,
    }

    #[async_trait::async_trait]
    impl BoundsPublisher for FakePublisher<'_> {
        async fn publish(&self, task: &ProbeTask) -> Result<()> {
            ensure!(
                self.store.exists("work", &task.state.svg_key).await?,
                "SVG was published before it was stored"
            );
            self.seen.lock().unwrap().push(task.clone());
            Ok(())
        }
    }

    #[tokio::test]
    async fn unique_svgs_are_stored_before_job_scoped_prefetch_is_published() {
        let root = tempfile::tempdir().unwrap();
        let store = FsStore(root.path().into());
        let publisher = FakePublisher {
            store: &store,
            seen: Mutex::new(Vec::new()),
        };
        let first = br#"<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"/>"#;
        let second = br#"<svg xmlns="http://www.w3.org/2000/svg" width="2" height="2"/>"#;
        let uploads = [first.as_slice(), second.as_slice()]
            .into_iter()
            .map(|bytes| {
                let state = StateRef::new(bytes);
                (state.svg_key, bytes.to_vec())
            })
            .collect();
        let prefetch = BoundsPrefetch {
            publisher: &publisher,
            resolution: 256,
            padding_pixels: 1,
        };
        let config = prefetch_config(prefetch, 1.0).unwrap();
        let job_id = "45cfafbd-5089-4f6d-850a-caa798ec1fcb";
        let prefetcher = SvgPrefetcher::new(&store, "work", prefetch, config, false, job_id);
        upload_and_prefetch(&store, "work", uploads, Some(&prefetcher))
            .await
            .unwrap();
        let stats = prefetcher.stats();
        assert_eq!((stats.published, stats.failed), (2, 0));
        let seen = publisher.seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert!(seen.iter().all(|task| {
            !task.cache_enabled
                && task.job_id.as_deref() == Some(job_id)
                && task
                    .result_key
                    .starts_with(&format!("jobs/{job_id}/prepare/bounds-results/"))
        }));
    }

    #[tokio::test]
    async fn live_stream_ignores_incomplete_files_and_deduplicates_svg_bytes() {
        let root = tempfile::tempdir().unwrap();
        let store = FsStore(root.path().join("objects"));
        let publisher = FakePublisher {
            store: &store,
            seen: Mutex::new(Vec::new()),
        };
        let prefetch = BoundsPrefetch {
            publisher: &publisher,
            resolution: 256,
            padding_pixels: 1,
        };
        let prefetcher = SvgPrefetcher::new(
            &store,
            "work",
            prefetch,
            prefetch_config(prefetch, 1.0).unwrap(),
            true,
            "job",
        );
        let output = root.path().join("ffdec");
        tokio::fs::create_dir_all(output.join("DefineSprite_1"))
            .await
            .unwrap();
        let complete = br#"<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"/>"#;
        tokio::fs::write(output.join("DefineSprite_1/1.svg"), complete)
            .await
            .unwrap();
        tokio::fs::write(output.join("DefineSprite_1/2.svg"), complete)
            .await
            .unwrap();
        tokio::fs::write(output.join("DefineSprite_1/3.svg"), b"<svg")
            .await
            .unwrap();

        let mut observed = BTreeSet::new();
        assert_eq!(
            stream_completed_svgs(&output, &mut observed, &prefetcher).await,
            1
        );
        assert_eq!(observed.len(), 2);
        assert_eq!(publisher.seen.lock().unwrap().len(), 1);

        let different = br#"<svg xmlns="http://www.w3.org/2000/svg" width="2" height="2"/>"#;
        tokio::fs::write(output.join("DefineSprite_1/3.svg"), different)
            .await
            .unwrap();
        assert_eq!(
            stream_completed_svgs(&output, &mut observed, &prefetcher).await,
            1
        );
        assert_eq!(observed.len(), 3);
        assert_eq!(publisher.seen.lock().unwrap().len(), 2);
    }

    #[test]
    fn filesystem_events_select_completed_svg_writes_and_rescans() {
        let svg = PathBuf::from("/tmp/export/frame.svg");
        assert!(svg_completion_event(
            &Event::new(EventKind::Access(AccessKind::Close(AccessMode::Write)))
                .add_path(svg.clone())
        ));
        assert!(svg_completion_event(
            &Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::To))).add_path(svg.clone())
        ));
        assert!(!svg_completion_event(
            &Event::new(EventKind::Access(AccessKind::Close(AccessMode::Read))).add_path(svg)
        ));
        assert!(!svg_completion_event(
            &Event::new(EventKind::Access(AccessKind::Close(AccessMode::Write)))
                .add_path(PathBuf::from("/tmp/export/frame.tmp"))
        ));
        assert!(svg_completion_event(
            &Event::new(EventKind::Other).set_flag(notify::event::Flag::Rescan)
        ));
    }

    #[test]
    fn scripts_keep_cc_stops_and_random_pose() {
        let mut value = ScriptMetadata::default();
        value.inspect(r#"package aq { public class Test { function Test() {addFrameScript(25,this.frame26);} function frame26():void {this.stop();} mcSetColor(this,"Base","dark"); gotoAndStop(Math.round(Math.random()*totalFrames)); }}"#).unwrap();
        assert_eq!(value.timelines["aq.test"].frames[&26].commands[0].action, crate::script::Action::Stop);
        assert_eq!(value.color_rules["aq.test"], vec!["Base", "dark"]);
        assert!(value.random_pose);
    }
    #[test]
    fn root_selection_ranges_are_deterministic() {
        assert_eq!(ranges(&[3, 1, 2, 3, 8, 9]), "1-3,8-9");
    }

    #[tokio::test]
    #[ignore = "requires AQW_TEST_FFDEC and AQW_TEST_MOGLIN_SWF; local FFDec regression, no AWS"]
    async fn real_moglin_export_keeps_idle_animated_without_walking() -> Result<()> {
        let jar = PathBuf::from(std::env::var("AQW_TEST_FFDEC")?);
        let bytes = std::fs::read(std::env::var("AQW_TEST_MOGLIN_SWF")?)?;
        ensure!(crate::sha256(&bytes) == "777ad52938825594c49c4b46af688314e4e44ad5eae9dc44ae2c5450601b33f6", "wrong regression asset");
        let root = tempfile::tempdir()?;
        let store = FsStore(root.path().join("objects"));
        let source = json!({"idx":0,"key":"pet.swf","sha256":crate::sha256(&bytes),"requests":[{"key":"pet","class_name":"QuibbleBFCM2024Pet","character_id":114,"frame":8,"root_timeline_frames":1}]});
        store.put("source", "pet.swf", bytes, "application/octet-stream", true).await?;
        store::write(&store,"work","input.json", &json!({"job_id":"fixture","sources":[source.clone()],"settings":{"zoom":1.0,"subframe_start":1},"export_frame_count":76}), false).await?;
        let publisher = FakePublisher { store: &store, seen: Mutex::new(Vec::new()) };
        let event = json!({"job_id":"fixture","input_key":"input.json","source":source});
        let options = || ExportOptions { jar: jar.clone(), timeout: Duration::from_secs(180), bounds_prefetch: Some(BoundsPrefetch { publisher: &publisher, resolution:256, padding_pixels:1 }) };
        let result = export_source(&store,"work","source",&event,options()).await?;
        let manifest: SourceManifest = store::read(&store,"work",result["manifest_key"].as_str().unwrap()).await?;
        assert_eq!(manifest.timeline_decisions.iter().find(|d| d.character_id == 113).unwrap().selection, crate::timeline::Selection::Hold { frame:16 });
        assert!(!manifest.timeline_decisions.iter().any(|d| d.character_id == 94), "37-frame idle artwork should remain untouched");
        assert!(manifest.states.len() > 1, "idle must not be frozen into a still image");
        assert_eq!(manifest.symbols["pet"].schedule.len(), 76);
        for state in manifest.states.values() {
            let svg = store.get("work", &state.svg_key).await?.unwrap();
            let doc = svg::parse(&svg)?;
            assert!(!doc.root.iter().any(|n| n.get_in("characterId",FFDEC_NS).and_then(|s|s.parse::<u16>().ok()).is_some_and(|id| (101..=112).contains(&id))), "walking artwork leaked into the idle export");
        }
        let mut pixels = BTreeSet::new();
        for frame in [0,15,37] {
            let state = &manifest.states[&manifest.symbols["pet"].schedule[frame]];
            let svg = store.get("work",&state.svg_key).await?.unwrap();
            let tree = resvg::usvg::Tree::from_data(&svg,&resvg::usvg::Options::default())?;
            let mut pixmap = resvg::tiny_skia::Pixmap::new(256,256).unwrap();
            let scale = 256.0/tree.size().width().max(tree.size().height());
            resvg::render(&tree,resvg::tiny_skia::Transform::from_scale(scale,scale),&mut pixmap.as_mut());
            assert!(pixmap.data().chunks_exact(4).any(|p|p[3]>0), "normalized idle rendered transparent");
            pixels.insert(crate::sha256(pixmap.data()));
        }
        assert!(pixels.len()>1, "different SVG hashes must correspond to actual visible animation");
        let queued: BTreeSet<_> = publisher.seen.lock().unwrap().iter().map(|t|t.state.sha256.clone()).collect();
        assert_eq!(queued, manifest.states.keys().cloned().collect(), "prefetch must publish only corrected SVGs");
        assert_eq!(export_source(&store,"work","source",&event,options()).await?["vector_cache_hit"], true);
        println!("moglin regression: 76 frames, {} unique animated idle SVGs, no walking IDs, cache hit verified", manifest.states.len());
        Ok(())
    }

    #[test]
    #[ignore = "requires AQW_TEST_GAUNTLET_DIR with saved SVG and decompiled scripts; local only"]
    fn actual_gauntlet_visibility_and_pixels() -> Result<()> {
        use aqw_component_raster::{import, component_svg, svg, raster};
        use std::collections::HashMap;
        let root = PathBuf::from(std::env::var("AQW_TEST_GAUNTLET_DIR")?);
        let mut meta = ScriptMetadata::default();
        for path in script_files(&root.join("6d63be22eea7105f80a998cb24795629300bfd6a27734ad60cb9bfae42c48137-scripts"))? {
            meta.inspect(&std::fs::read_to_string(path)?)?;
        }
        assert_eq!(meta.hand_visibility.len(),2);
        let rules: HashMap<_,_> = meta.hand_visibility.into_iter().collect();
        let colors: HashMap<_,_> = meta.color_rules.into_iter().map(|(k,v)|(k,(v[0].clone(),v[1].clone()))).collect();
        let source = std::fs::read_to_string(root.join("gauntlet_front.svg"))?;
        let mut pixels = Vec::new();
        for (label, hand) in [("before",None),("front",Some("fronthand")),("back",Some("backhand"))] {
            let imported = import::import_ffdec_symbol_with_visibility("weapon",&source,1.0,&colors,"FurryofRisen",&HashMap::new(),Some(14),&rules,hand)?;
            let count = imported.definition.children[0].children.len();
            assert_eq!(count, if hand.is_none(){2}else{1});
            let all_rules = colors.values().cloned().collect::<Vec<_>>();
            let built = component_svg::build_component_svg(&imported,import::IDENTITY,false,"weapon",label,imported.bounds,700,&HashMap::from([("intColorAccessory".into(),"16711680".into()),("intColorBase".into(),"16711680".into()),("intColorTrim".into(),"16711680".into())]),&all_rules);
            let encoded = svg::serialize(&svg::Document{root:built.root,namespaces:built.namespaces});
            let image = raster::render_svg_resvg(encoded.as_bytes(),((built.page[2]-built.page[0]) as u32,(built.page[3]-built.page[1]) as u32))?;
            std::fs::write(root.join(format!("{label}.png")), raster::encode_rgba8(image.width,image.height,&image.pixels)?)?;
            pixels.push(image.pixels);
        }
        assert_ne!(pixels[0],pixels[1]);
        assert_ne!(pixels[0],pixels[2]);
        assert_ne!(pixels[1],pixels[2]);
        Ok(())
    }

    #[test]
    #[ignore = "requires AQW_TEST_SCRIPT_CORPUS with input.json, source/ and scripts paths; no AWS"]
    fn saved_scripts_resolve_full_source_context() -> Result<()> {
        let root = PathBuf::from(std::env::var("AQW_TEST_SCRIPT_CORPUS")?);
        let input: Value = serde_json::from_slice(&std::fs::read(root.join("input.json"))?)?;
        for source in input["sources"].as_array().context("missing sources")? {
            let bytes = std::fs::read(root.join("source").join(source["key"].as_str().unwrap()))?;
            let swf = Swf::parse(&bytes)?;
            let mut requests: Vec<SymbolRequest> = serde_json::from_value(source.get("normalization_requests").unwrap_or(&source["requests"]).clone())?;
            for request in &mut requests { request.frame = swf.timeline(request.character_id)?.0; }
            let mut metadata = ScriptMetadata::default();
            for path in script_files(Path::new(source["scripts"].as_str().unwrap()))? {
                metadata.inspect(&std::fs::read_to_string(&path)?).with_context(|| format!("script {}", path.display()))?;
            }
            let normalized = crate::timeline::normalize(&bytes, &swf, &metadata.timelines, &requests)?;
            Swf::parse(&normalized.bytes)?;
            assert_eq!(normalized.requests.len(), requests.len());
            if requests.iter().any(|r| r.class_name.starts_with("SteampunkLandshipUnitArmor") && r.key == "armor_head") {
                assert!(normalized.decisions.iter().any(|d| d.character_id == 469 && d.selection == crate::timeline::Selection::Hold { frame: 2 }));
            }
            println!("full context {}: {} roots, decisions {:?}",source["remote_path"], requests.len(), normalized.decisions);
        }
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires AQW_TEST_FFDEC and AQW_TEST_SOURCE_CORPUS containing input.json and source/; no AWS"]
    async fn real_source_corpus_resolves_before_export() -> Result<()> {
        let root = PathBuf::from(std::env::var("AQW_TEST_SOURCE_CORPUS")?);
        let input: Value = serde_json::from_slice(&std::fs::read(root.join("input.json"))?)?;
        let ffdec = Ffdec { jar: std::env::var("AQW_TEST_FFDEC")?.into(), deadline: Instant::now() + Duration::from_secs(180) };
        let temporary = tempfile::tempdir()?;
        for (index, source) in input["sources"].as_array().context("missing corpus sources")?.iter().enumerate() {
            let path = root.join("source").join(source["key"].as_str().context("missing source key")?);
            let bytes = std::fs::read(&path)?;
            let swf = Swf::parse(&bytes)?;
            let mut requests: Vec<SymbolRequest> = serde_json::from_value(source["requests"].clone())?;
            for r in &mut requests { r.frame = swf.timeline(r.character_id)?.0; }
            let output = temporary.path().join(index.to_string());
            ffdec.run(&temporary.path().join("home"), &["-onerror".into(),"ignore".into(),"-export".into(),"script".into(),output.to_string_lossy().into_owned(),path.to_string_lossy().into_owned()]).await?;
            let mut metadata = ScriptMetadata::default();
            for p in script_files(&output)? { metadata.inspect(&std::fs::read_to_string(p)?)?; }
            let normalized = crate::timeline::normalize(&bytes,&swf,&metadata.timelines,&requests).with_context(||format!("source {}",path.display()))?;
            Swf::parse(&normalized.bytes)?;
            println!("corpus {}: {} requested roots; {} timeline corrections", path.display(), requests.len(), normalized.decisions.len());
        }
        Ok(())
    }

}
