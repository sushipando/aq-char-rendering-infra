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
    pub inert_background_actions: BTreeSet<String>,
    #[serde(default)]
    pub hand_visibility: BTreeMap<String, String>,
    pub color_rules: BTreeMap<String, Vec<String>>,
    pub timelines: BTreeMap<String, crate::script::Class>,
    pub random_pose: bool,
}

impl ScriptMetadata {
    /// Shared production policy for local diagnostics and AWS export.
    pub fn normalize(
        &self,
        source: &[u8],
        swf: &Swf,
        requests: &[SymbolRequest],
    ) -> Result<crate::timeline::Normalized> {
        let mut allowed_actions = BTreeSet::new();
        if !self.inert_background_actions.is_empty()
            && requests.iter().all(|r| r.key == crate::background::KEY)
        {
            let body = crate::swf::decompress(source)?;
            let stage_offset = (5 + 4 * (body[0] as usize >> 3)).div_ceil(8) + 4;
            let as3 = crate::swf::tags(&body, stage_offset)?
                .iter()
                .any(|(code, data)| {
                    *code == 69 && data.first().is_some_and(|flags| flags & 8 != 0)
                });
            if !as3 {
                allowed_actions.clone_from(&self.inert_background_actions);
            }
        }
        let (source, scripts) = crate::animate::prepare(source, swf, &self.timelines)?;
        let swf = Swf::parse(&source)?;
        crate::timeline::normalize_with_avm1(&source, &swf, &scripts, requests, &allowed_actions)
    }

    pub fn inspect_file(&mut self, path: &Path, swf: &Swf) -> Result<()> {
        let text = std::fs::read_to_string(path)?;
        self.inspect(&text)?;
        // FFDec's AVM1 filenames identify a sprite and frame, not an AS3 class.
        // Only approve an unambiguous, single DoAction on that exact frame and
        // bind approval to its bytes. Missing/unrecognized scripts stay rejected.
        if path.file_name().and_then(|s| s.to_str()) != Some("DoAction.as")
            || !crate::script::inert_background_avm1(&text)? { return Ok(()); }
        let frame_dir = path.parent().context("missing AVM1 frame directory")?;
        let frame: usize = frame_dir.file_name().and_then(|s| s.to_str())
            .and_then(|s| s.strip_prefix("frame_")).context("invalid AVM1 frame path")?.parse()?;
        let id: u16 = frame_dir.parent().and_then(Path::file_name).and_then(|s| s.to_str())
            .and_then(|s| s.strip_prefix("DefineSprite_")).context("invalid AVM1 sprite path")?.parse()?;
        let payload = swf.sprites.get(&id).context("missing AVM1 sprite")?;
        let mut current = 1;
        let mut actions = Vec::new();
        for (code, data) in crate::swf::tags(payload, 4)? {
            if code == 12 && current == frame { actions.push(data); }
            if code == 1 { current += 1; }
        }
        ensure!(actions.len() == 1, "ambiguous AVM1 frame script");
        self.inert_background_actions.insert(crate::sha256(actions[0]));
        Ok(())
    }

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
        source
            .get("normalization_requests")
            .unwrap_or(&source["requests"])
            .clone(),
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
    tokio::fs::write(&source_path, crate::swf::repair_missing_end(&bytes)?).await?;
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
    let mut metadata = if let Some(cached) = cached_metadata {
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
            computed
                .inspect_file(&path, &swf)
                .with_context(|| format!("decompiled script {}", path.display()))?;
        }
        if vector_cache {
            store::write(store, work_bucket, &metadata_key, &computed, true).await?;
        }
        computed
    };
    let mut normalization_requests = normalization_requests;
    for request in &mut normalization_requests {
        request.capture_end = start + count - 1;
    }
    let normalized = metadata.normalize(&bytes, &swf, &normalization_requests)?;
    let selected: BTreeSet<_> = requests.iter().map(|r| r.key.as_str()).collect();
    let effective_requests: Vec<_> = normalized
        .requests
        .iter()
        .filter(|r| selected.contains(r.key.as_str()))
        .cloned()
        .collect();
    ensure!(
        effective_requests.len() == requests.len(),
        "export unit is missing from normalization context"
    );
    tokio::fs::write(&source_path, &normalized.bytes).await?;
    crate::log(
        "export_timeline_resolution",
        json!({"job_id":job,"source_idx":source_idx,"decisions":normalized.decisions}),
    );
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
    for (alias, original) in &normalized.symbol_aliases {
        if let Some(rules) = metadata.color_rules.get(original).cloned() {
            metadata.color_rules.insert(alias.clone(), rules);
        }
        if let Some(hand) = metadata.hand_visibility.get(original).cloned() {
            metadata.hand_visibility.insert(alias.clone(), hand);
        }
    }
    let mut manifest = SourceManifest {
        schema_version: VECTOR_SCHEMA,
        export_policy: EXPORT_POLICY.into(),
        source_sha256: source_hash.into(),
        export_identity: identity,
        symbols: BTreeMap::new(),
        states: BTreeMap::new(),
        hand_visibility: metadata.hand_visibility,
        color_rules: metadata.color_rules,
        placement_colors: Swf::parse(&normalized.bytes)?.placement_colors()?,
        timeline_decisions: normalized.decisions.clone(),
        host_visibility: normalized.host_visibility.clone(),
        timeline_warnings: normalized.warnings.clone(),
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
        let effective = normalized
            .requests
            .iter()
            .find(|r| r.key == request.key)
            .context("missing normalized export request")?
            .clone();
        let stop = normalized
            .decisions
            .iter()
            .find(|d| d.character_id == effective.character_id)
            .and_then(|d| match d.selection {
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
    #[ignore = "requires AQW_TEST_FFDEC and AQW_TEST_LOCAL_CORPUS; local only"]
    async fn real_generalized_timeline_exports_preserve_pixels_and_click_animation() -> Result<()> {
        let corpus = PathBuf::from(std::env::var("AQW_TEST_LOCAL_CORPUS")?);
        let root = tempfile::Builder::new()
            .prefix("aqw-generalized-visual-")
            .tempdir()?
            .keep();
        println!("visual regression artifacts: {}", root.display());
        let store = FsStore(root.join("objects"));
        let mut click_results = Vec::new();
        for (index, (path, suffix, click)) in [
            (
                "classes/F/DarkBloodEviscerater.swf",
                "DarkBloodEvisceraterFHand",
                false,
            ),
            (
                "classes/F/DarkBloodEviscerater.swf",
                "DarkBloodEvisceraterFHand",
                true,
            ),
            ("items/polearms/Lanceofcthulhu.swf", "Lanceofcthulhu", false),
            (
                "items/pets/MutantShadowDragonPet.swf",
                "MutantShadowDragonPet",
                false,
            ),
            ("items/swords/ArchFMageSword.swf", "ArchFMageSword", false),
            (
                "items/Capes/ScarlettaNCMirrorC2.swf",
                "ScarlettaNCMirrorC2",
                false,
            ),
            ("items/Gauntlets/dBSGauntletsr2.swf", "dBSGauntlets", false),
            (
                "items/daggers/Kaharakasandknightslasherdaggers.swf",
                "Kaharakasandknightslasherdaggers",
                false,
            ),
            ("items/pets/AuraMaxingPetr1.swf", "AuraMaxingPet", false),
            (
                "items/swords/SoulDevourerBlade.swf",
                "SoulDevourerBlade",
                false,
            ),
        ]
        .into_iter()
        .enumerate()
        {
            let bytes = std::fs::read(corpus.join(path))?;
            let swf = Swf::parse(&bytes)?;
            let (id, name) = swf.symbol(suffix).context("missing real regression root")?;
            let (frame, frames) = swf.timeline(id)?;
            let key = format!("case{index}");
            let source = json!({"idx":0,"key":key,"sha256":crate::sha256(&bytes),"requests":[{"key":key,"class_name":name,"character_id":id,"frame":frame,"root_timeline_frames":frames,"click":click,"ancestor_names":["head","mcChar","stage"]}]});
            store
                .put("source", &key, bytes, "application/octet-stream", true)
                .await?;
            store::write(&store,"work",&key,&json!({"job_id":key,"sources":[source.clone()],"settings":{"zoom":1.0,"subframe_start":1},"export_frame_count":36}),false).await?;
            let result = export_source(
                &store,
                "work",
                "source",
                &json!({"job_id":key,"input_key":key,"source":source}),
                ExportOptions::without_prefetch(
                    std::env::var("AQW_TEST_FFDEC")?.into(),
                    Duration::from_secs(240),
                ),
            )
            .await?;
            let manifest: SourceManifest =
                store::read(&store, "work", result["manifest_key"].as_str().unwrap()).await?;
            let mut pixels = BTreeSet::new();
            let mut nonempty = 0;
            for (n, state) in manifest.states.values().enumerate() {
                let svg = store.get("work", &state.svg_key).await?.unwrap();
                let tree = resvg::usvg::Tree::from_data(&svg, &resvg::usvg::Options::default())?;
                let mut pixmap = resvg::tiny_skia::Pixmap::new(384, 384).unwrap();
                let scale = 384.0 / tree.size().width().max(tree.size().height());
                resvg::render(
                    &tree,
                    resvg::tiny_skia::Transform::from_scale(scale, scale),
                    &mut pixmap.as_mut(),
                );
                nonempty += usize::from(pixmap.data().chunks_exact(4).any(|p| p[3] > 0));
                pixels.insert(crate::sha256(pixmap.data()));
                if n < 3 {
                    pixmap.save_png(root.join(format!("{key}-{n}.png")))?;
                }
            }
            ensure!(nonempty > 0, "empty real render for {path}");
            if index < 2 {
                click_results.push(pixels.clone());
            }
            println!(
                "{path}, click={click}: {} distinct raster states",
                pixels.len()
            );
        }
        ensure!(
            click_results[0] != click_results[1],
            "click must change the real hand animation"
        );
        println!("visual regression artifacts: {}", root.display());
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires AQW_TEST_FFDEC and AQW_TEST_LOCAL_CORPUS; local only"]
    async fn real_bank_cape_and_chest_export_keep_idle_artwork() -> Result<()> {
        let corpus = PathBuf::from(std::env::var("AQW_TEST_LOCAL_CORPUS")?);
        let root = tempfile::tempdir()?;
        let store = FsStore(root.path().join("objects"));
        for (path, class, key) in [
            (
                "items/Capes/ALCThroneBankCape.swf",
                "ALCThroneBankCape",
                "cape",
            ),
            (
                "items/pets/16Birthday10kCollectionChest.swf",
                "16Birthday10kCollectionChest",
                "pet",
            ),
        ] {
            let bytes = std::fs::read(corpus.join(path))?;
            let swf = Swf::parse(&bytes)?;
            let (id, name) = swf.symbol(class).context("missing regression root")?;
            let (frame, frames) = swf.timeline(id)?;
            let source = json!({"idx":0,"key":key,"sha256":crate::sha256(&bytes),"requests":[{"key":key,"class_name":name,"character_id":id,"frame":frame,"root_timeline_frames":frames}]});
            store
                .put("source", key, bytes, "application/octet-stream", true)
                .await?;
            store::write(&store,"work",key,&json!({"job_id":key,"sources":[source.clone()],"settings":{"zoom":1.0,"subframe_start":1},"export_frame_count":12}),false).await?;
            let event = json!({"job_id":key,"input_key":key,"source":source});
            let options = || {
                ExportOptions::without_prefetch(
                    std::env::var("AQW_TEST_FFDEC").unwrap().into(),
                    Duration::from_secs(180),
                )
            };
            let result = export_source(&store, "work", "source", &event, options()).await?;
            let manifest: SourceManifest =
                store::read(&store, "work", result["manifest_key"].as_str().unwrap()).await?;
            assert_eq!(manifest.symbols[key].schedule.len(), 12);
            assert!(manifest
                .timeline_decisions
                .iter()
                .any(|d| d.character_id == id
                    && d.selection == crate::timeline::Selection::Hold { frame }));
            if key == "pet" {
                assert!(
                    manifest
                        .timeline_decisions
                        .iter()
                        .any(|d| d.class_name.ends_with("CCPet_2")
                            && d.selection == crate::timeline::Selection::Hold { frame: 16 }),
                    "chest child must settle on authored idle, not walking"
                );
            }
            let mut pixels = BTreeSet::new();
            for state in manifest.states.values() {
                let svg = store.get("work", &state.svg_key).await?.unwrap();
                let tree = resvg::usvg::Tree::from_data(&svg, &resvg::usvg::Options::default())?;
                let mut pixmap = resvg::tiny_skia::Pixmap::new(256, 256).unwrap();
                let scale = 256.0 / tree.size().width().max(tree.size().height());
                resvg::render(
                    &tree,
                    resvg::tiny_skia::Transform::from_scale(scale, scale),
                    &mut pixmap.as_mut(),
                );
                assert!(pixmap.data().chunks_exact(4).any(|p| p[3] > 0));
                pixels.insert(crate::sha256(pixmap.data()));
            }
            assert!(
                pixels.len() > 1,
                "bank UI initialization must not freeze nested idle effects"
            );
            assert_eq!(
                export_source(&store, "work", "source", &event, options()).await?
                    ["vector_cache_hit"],
                true
            );
            println!(
                "{path}: {} distinct raster states; decisions {:?}",
                pixels.len(),
                manifest.timeline_decisions
            );
        }
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires AQW_TEST_FFDEC and AQW_TEST_LOCAL_CORPUS; local only"]
    async fn real_conditional_batch_keeps_flames_animated_and_weapons_idle() -> Result<()> {
        let corpus = PathBuf::from(std::env::var("AQW_TEST_LOCAL_CORPUS")?);
        let root = tempfile::tempdir()?;
        let store = FsStore(root.path().join("objects"));
        for (path, class, key, count) in [
            (
                "items/Helms/2016DCBlackSpiritHead.swf",
                "2016DCBlackSpiritHead",
                "helm",
                12,
            ),
            (
                "items/swords/ApocrphyalGreatsword.swf",
                "ApocrphyalGreatsword",
                "weapon",
                1,
            ),
        ] {
            let bytes = std::fs::read(corpus.join(path))?;
            let swf = Swf::parse(&bytes)?;
            let (id, name) = swf.symbol(class).context("missing regression root")?;
            let source = json!({"idx":0,"key":key,"sha256":crate::sha256(&bytes),"requests":[{"key":key,"class_name":name,"character_id":id,"frame":1,"root_timeline_frames":1}]});
            store
                .put("source", key, bytes, "application/octet-stream", true)
                .await?;
            store::write(&store,"work",key,&json!({"job_id":key,"sources":[source.clone()],"settings":{"zoom":1.0,"subframe_start":1},"export_frame_count":count}),false).await?;
            let event = json!({"job_id":key,"input_key":key,"source":source});
            let options = || {
                ExportOptions::without_prefetch(
                    std::env::var("AQW_TEST_FFDEC").unwrap().into(),
                    Duration::from_secs(180),
                )
            };
            let result = export_source(&store, "work", "source", &event, options()).await?;
            let manifest: SourceManifest =
                store::read(&store, "work", result["manifest_key"].as_str().unwrap()).await?;
            let mut pixels = BTreeSet::new();
            for state in manifest.states.values() {
                let svg = store.get("work", &state.svg_key).await?.unwrap();
                let tree = resvg::usvg::Tree::from_data(&svg, &resvg::usvg::Options::default())?;
                let mut pixmap = resvg::tiny_skia::Pixmap::new(256, 256).unwrap();
                let scale = 256.0 / tree.size().width().max(tree.size().height());
                resvg::render(
                    &tree,
                    resvg::tiny_skia::Transform::from_scale(scale, scale),
                    &mut pixmap.as_mut(),
                );
                assert!(pixmap.data().chunks_exact(4).any(|p| p[3] > 0));
                pixels.insert(crate::sha256(pixmap.data()));
            }
            if key == "helm" {
                assert!(
                    pixels.len() > 1,
                    "randomized flame initialization must not freeze animation"
                );
                assert!(!manifest
                    .timeline_decisions
                    .iter()
                    .any(|d| d.class_name.contains("BasicFire")
                        && matches!(d.selection, crate::timeline::Selection::Hold { .. })));
            } else {
                assert!(manifest
                    .timeline_decisions
                    .iter()
                    .any(|d| d.class_name.ends_with("Weapon_2")
                        && d.selection == crate::timeline::Selection::Hold { frame: 1 }));
            }
            assert_eq!(
                export_source(&store, "work", "source", &event, options()).await?
                    ["vector_cache_hit"],
                true
            );
            println!("{path}: {} distinct raster states", pixels.len());
        }
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires AQW_TEST_FFDEC and AQW_TEST_BATTLEON_WRAPPED_SWF; local only"]
    async fn real_battleon_background_linkage_normalizes() -> Result<()> {
        let source = PathBuf::from(std::env::var("AQW_TEST_BATTLEON_WRAPPED_SWF")?);
        let bytes = std::fs::read(&source)?;
        ensure!(
            crate::sha256(&bytes)
                == "57a1fe4b69af41a4e1bb66fb1997fa93e9c01a7b1fdbedad2a49524520cac6b7",
            "wrong regression asset"
        );
        let swf = Swf::parse(&bytes)?;
        let root = tempfile::tempdir()?;
        let output = root.path().join("scripts");
        Ffdec {
            jar: std::env::var("AQW_TEST_FFDEC")?.into(),
            deadline: Instant::now() + Duration::from_secs(90),
        }
        .run(
            &root.path().join("home"),
            &[
                "-export".into(),
                "script".into(),
                output.to_string_lossy().into_owned(),
                source.to_string_lossy().into_owned(),
            ],
        )
        .await?;
        let mut metadata = ScriptMetadata::default();
        for path in script_files(&output)? {
            metadata.inspect_file(&path, &swf)?;
        }
        assert_eq!(metadata.inert_background_actions.len(), 1);
        let request = SymbolRequest {
            key: crate::background::KEY.into(),
            class_name: "CharpageBackgroundStage".into(),
            character_id: 65533,
            frame: 1,
            root_timeline_frames: 1,
            click: false,
            ancestor_names: Vec::new(),
            capture_end: 2008,
        };
        let normalized = metadata.normalize(&bytes, &swf, &[request.clone()])?;
        assert_eq!(
            normalized.bytes, bytes,
            "background geometry and timelines must remain unchanged"
        );
        let mut item = request;
        item.key = "cape".into();
        assert!(
            metadata.normalize(&bytes, &swf, &[item]).is_err(),
            "exception must remain background-only"
        );
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires AQW_TEST_FFDEC and AQW_TEST_BOA_SWF; local only"]
    async fn real_static_animate_layer_export_preserves_authored_effects() -> Result<()> {
        let bytes=std::fs::read(std::env::var("AQW_TEST_BOA_SWF")?)?;
        ensure!(crate::sha256(&bytes)=="5597b4544d0bd72afccb7e8cd7d7c785f81dc7f31aa9bf9de1eebeeb6c5e8b62","wrong regression asset");
        let root=tempfile::tempdir()?;
        let store=FsStore(root.path().join("objects"));
        let source=json!({"idx":0,"key":"boa.swf","sha256":crate::sha256(&bytes),"requests":[{"key":"weapon","class_name":"BoAEnergy","character_id":26,"frame":1,"root_timeline_frames":1}]});
        store.put("source","boa.swf",bytes.clone(),"application/octet-stream",true).await?;
        store::write(&store,"work","input.json",&json!({"job_id":"boa","sources":[source.clone()],"settings":{"zoom":1.0,"subframe_start":1},"export_frame_count":1}),false).await?;
        let event=json!({"job_id":"boa","input_key":"input.json","source":source});
        let options=|| ExportOptions::without_prefetch(std::env::var("AQW_TEST_FFDEC").unwrap().into(),Duration::from_secs(180));
        let result=export_source(&store,"work","source",&event,options()).await?;
        let manifest:SourceManifest=store::read(&store,"work",result["manifest_key"].as_str().unwrap()).await?;
        let meta:ScriptMetadata=store::read(&store,"work",&format!("source-metadata/{EXPORT_POLICY}/{FFDEC_VERSION}/{}.json",crate::sha256(&bytes))).await?;
        let swf=Swf::parse(&bytes)?;
        let requests:Vec<SymbolRequest>=serde_json::from_value(event["source"]["requests"].clone())?;
        let normalized=meta.normalize(&bytes,&swf,&requests)?;
        assert_eq!(normalized.bytes,bytes,"all authored filters, blends, placements and geometry must remain intact");
        assert_eq!(manifest.states.len(),1);
        let state=manifest.states.values().next().unwrap();
        let svg=store.get("work",&state.svg_key).await?.unwrap();
        assert!(std::str::from_utf8(&svg)?.contains("filter"),"authored glow effects missing from SVG");
        let tree=resvg::usvg::Tree::from_data(&svg,&resvg::usvg::Options::default())?;
        let mut pixmap=resvg::tiny_skia::Pixmap::new(256,256).unwrap();
        let scale=256.0/tree.size().width().max(tree.size().height());
        resvg::render(&tree,resvg::tiny_skia::Transform::from_scale(scale,scale),&mut pixmap.as_mut());
        assert!(pixmap.data().chunks_exact(4).any(|p|p[3]>0));
        if let Ok(path)=std::env::var("AQW_TEST_BOA_PREVIEW") {pixmap.save_png(path)?;}
        assert_eq!(export_source(&store,"work","source",&event,options()).await?["vector_cache_hit"],true);
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires AQW_TEST_FFDEC, AQW_TEST_SCYTHE_SWF, AQW_TEST_BG18_WRAPPED_SWF; no AWS"]
    async fn real_escaped_scythe_and_avm1_background_export() -> Result<()> {
        let root = tempfile::tempdir()?;
        let store = FsStore(root.path().join("objects"));
        for (env, key, id, class, count) in [
            ("AQW_TEST_SCYTHE_SWF", "weapon", 14, "013BlackSkullsScythe", 1),
            ("AQW_TEST_BG18_WRAPPED_SWF", crate::background::KEY, 65533, "CharpageBackgroundStage", 50),
        ] {
            let bytes = std::fs::read(std::env::var(env)?)?;
            let source = json!({"idx":0,"key":key,"sha256":crate::sha256(&bytes),"requests":[{"key":key,"class_name":class,"character_id":id,"frame":1,"root_timeline_frames":1}]});
            store.put("source",key,bytes.clone(),"application/octet-stream",true).await?;
            store::write(&store,"work",key,&json!({"job_id":key,"sources":[source.clone()],"settings":{"zoom":1.0,"subframe_start":1},"export_frame_count":count}),false).await?;
            let event = json!({"job_id":key,"input_key":key,"source":source});
            let options = || ExportOptions {jar:std::env::var("AQW_TEST_FFDEC").unwrap().into(),timeout:Duration::from_secs(240),bounds_prefetch:None};
            let result = export_source(&store,"work","source",&event,options()).await?;
            let manifest: SourceManifest = store::read(&store,"work",result["manifest_key"].as_str().unwrap()).await?;
            assert_eq!(manifest.symbols[key].schedule.len(),count);
            assert!(!manifest.states.is_empty());
            for state in manifest.states.values() {
                let svg = store.get("work",&state.svg_key).await?.unwrap();
                let tree = resvg::usvg::Tree::from_data(&svg,&resvg::usvg::Options::default())?;
                let mut pixels = resvg::tiny_skia::Pixmap::new(128,128).unwrap();
                resvg::render(&tree,resvg::tiny_skia::Transform::from_scale(128.0/tree.size().width().max(tree.size().height()),128.0/tree.size().width().max(tree.size().height())),&mut pixels.as_mut());
                assert!(pixels.data().chunks_exact(4).any(|p|p[3]>0));
            }
            let metadata_key = format!("source-metadata/{EXPORT_POLICY}/{FFDEC_VERSION}/{}.json",crate::sha256(&bytes));
            let meta: ScriptMetadata = store::read(&store,"work",&metadata_key).await?;
            if key == crate::background::KEY {
                assert_eq!(meta.inert_background_actions.len(),2); // two identical color hooks
                let swf = Swf::parse(&bytes)?;
                let requests: Vec<SymbolRequest> = serde_json::from_value(event["source"]["requests"].clone())?;
                assert!(crate::timeline::normalize(&bytes,&swf,&meta.timelines,&requests).is_err(),"non-background exports must still reject AVM1");
                let normalized = crate::timeline::normalize_with_avm1(&bytes,&swf,&meta.timelines,&requests,&meta.inert_background_actions)?;
                let updated = Swf::parse(&normalized.bytes)?;
                assert_eq!(updated.sprites[&52],swf.sprites[&52],"authored 50-frame timeline must be untouched");
                assert_eq!(normalized.bytes,bytes,"background artwork, colors and all timelines must be unchanged");
            } else {
                assert!(meta.timelines.contains_key("013blackskullsscythe"));
            }
            assert_eq!(export_source(&store,"work","source",&event,options()).await?["vector_cache_hit"],true);
            println!("{key}: {count} frames, {} SVG states, nontransparent raster and warm cache verified",manifest.states.len());
        }
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires AQW_TEST_BACKGROUND_DIR and AQW_TEST_FFDEC; exact official SWFs, no AWS"]
    async fn real_background_timelines_preserve_motion() -> Result<()> {
        let root = tempfile::tempdir()?;
        let store = FsStore(root.path().join("objects"));
        for index in [32,34] {
            let original = std::fs::read(PathBuf::from(std::env::var("AQW_TEST_BACKGROUND_DIR")?).join(format!("bg{index}.swf")))?;
            let catalog = crate::background::record(index).unwrap();
            ensure!(crate::sha256(&original) == catalog["sha256"].as_str().unwrap(), "wrong background fixture");
            let (bytes,id,period) = crate::background::wrap(&original)?;
            assert!(period.is_none_or(|n| n > 1));
            let source = json!({"idx":index,"key":format!("bg{index}.swf"),"sha256":crate::sha256(&bytes),"requests":[{"key":crate::background::KEY,"class_name":"CharpageBackgroundStage","character_id":id,"frame":1,"root_timeline_frames":1}]});
            store.put("source",source["key"].as_str().unwrap(),bytes,"application/octet-stream",true).await?;
            let job = format!("bg{index}");
            let input = format!("{job}.json");
            store::write(&store,"work",&input,&json!({"job_id":job,"sources":[source.clone()],"settings":{"zoom":1.0,"subframe_start":1},"export_frame_count":210}),false).await?;
            let event = json!({"job_id":job,"input_key":input,"source":source});
            let options = || ExportOptions {jar:std::env::var("AQW_TEST_FFDEC").unwrap().into(),timeout:Duration::from_secs(240),bounds_prefetch:None};
            let result = export_source(&store,"work","source",&event,options()).await?;
            let manifest:SourceManifest = store::read(&store,"work",result["manifest_key"].as_str().unwrap()).await?;
            assert_eq!(manifest.symbols[crate::background::KEY].schedule.len(),210);
            assert!(manifest.states.len()>1,"background animation froze");
            let mut pixels = BTreeSet::new();
            for frame in [0,15,60,90] {
                let state = &manifest.states[&manifest.symbols[crate::background::KEY].schedule[frame]];
                let svg = store.get("work",&state.svg_key).await?.unwrap();
                if let Ok(dir) = std::env::var("AQW_TEST_BACKGROUND_PREVIEWS") { std::fs::create_dir_all(&dir)?; std::fs::write(PathBuf::from(dir).join(format!("bg{index}-{frame}.svg")),&svg)?; }
                // FFDec sprite SVGs are tightly cropped, whereas backgrounds use
                // their authored stage origin. Import removes that crop matrix.
                let imported = aqw_component_raster::import::import_ffdec_symbol(crate::background::KEY, std::str::from_utf8(&svg)?,1.0,&Default::default(),"CharpageBackgroundStage",&Default::default(),Some(id as i64))?;
                let component = aqw_component_raster::component_svg::build_component_svg(&imported,[1.0,0.0,0.0,1.0,0.0,0.0],false,"background",crate::background::KEY,[0.0,0.0,550.0,350.0],550,&Default::default(),&[]);
                let mut doc = svg::Document {root:component.root,namespaces:component.namespaces};
                doc.root.set("viewBox","0 0 550 350");
                doc.root.set("width","550"); doc.root.set("height","350");
                let tree = resvg::usvg::Tree::from_data(svg::serialize(&doc).as_bytes(),&resvg::usvg::Options::default())?;
                let mut pixmap = resvg::tiny_skia::Pixmap::new(275,175).unwrap();
                resvg::render(&tree,resvg::tiny_skia::Transform::from_scale(0.5,0.5),&mut pixmap.as_mut());
                assert!(pixmap.data().chunks_exact(4).any(|p|p[3]>0));
                pixels.insert(crate::sha256(pixmap.data()));
                if let Ok(dir) = std::env::var("AQW_TEST_BACKGROUND_PREVIEWS") { std::fs::create_dir_all(&dir)?; pixmap.save_png(PathBuf::from(dir).join(format!("bg{index}-{frame}.png")))?; }
            }
            assert!(pixels.len()>1,"SVG differences must produce visible motion");
            assert_eq!(export_source(&store,"work","source",&event,options()).await?["vector_cache_hit"],true);
            println!("background {index}: period {period:?}, {} SVG states, {} sampled pixel states",manifest.states.len(),pixels.len());
        }
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires AQW_TEST_FFDEC and AQW_TEST_BANK_DUCK_SWF; local FFDec regression, no AWS"]
    async fn real_bank_duck_exports_idle_and_reuses_cache() -> Result<()> {
        bank_pet_export_regression("AQW_TEST_BANK_DUCK_SWF", "641431a984dd3f1cfe84ac0524eebb037796367462d1092d4bae9bc4a0021cab", "FrostvalWaddlesTheBankDuck", 21).await
    }

    #[tokio::test]
    #[ignore = "requires AQW_TEST_FFDEC and AQW_TEST_BANK_QUIBBLE_SWF; local FFDec regression, no AWS"]
    async fn real_bank_quibble_exports_idle_and_reuses_cache() -> Result<()> {
        bank_pet_export_regression("AQW_TEST_BANK_QUIBBLE_SWF", "2843f1a4d086f49cee561d0de574ec96b730ecd8b93519339e9e6adcdab2ecff", "QuibXmasBank", 43).await
    }

    #[tokio::test]
    #[ignore = "requires AQW_TEST_FFDEC and AQW_TEST_BANK_ALVARO_SWF; local FFDec regression, no AWS"]
    async fn real_bank_alvaro_exports_idle_and_reuses_cache() -> Result<()> {
        bank_pet_export_regression("AQW_TEST_BANK_ALVARO_SWF", "5ac2e45bcbde94084b93cdd90dd016174c5ffabf2ea720414d27904a49a97590", "AlvaroPetNXBank", 104).await
    }

    async fn bank_pet_export_regression(asset_env: &str, sha: &str, class_name: &str, character_id: u16) -> Result<()> {
        let jar = PathBuf::from(std::env::var("AQW_TEST_FFDEC")?);
        let bytes = std::fs::read(std::env::var(asset_env)?)?;
        ensure!(crate::sha256(&bytes) == sha, "wrong regression asset");
        let root = tempfile::tempdir()?;
        let store = FsStore(root.path().join("objects"));
        let source = json!({"idx":3,"key":"pet.swf","sha256":crate::sha256(&bytes),"requests":[{"key":"pet","class_name":class_name,"character_id":character_id,"frame":8,"root_timeline_frames":1}]});
        store.put("source", "pet.swf", bytes, "application/octet-stream", true).await?;
        store::write(&store,"work","input.json", &json!({"job_id":"bank-pet-fixture","sources":[source.clone()],"settings":{"zoom":1.0,"subframe_start":1},"export_frame_count":120}), false).await?;
        let event = json!({"job_id":"bank-pet-fixture","input_key":"input.json","source":source});
        let options = || ExportOptions { jar: jar.clone(), timeout: Duration::from_secs(180), bounds_prefetch: None };
        let result = export_source(&store,"work","source",&event,options()).await?;
        let manifest: SourceManifest = store::read(&store,"work",result["manifest_key"].as_str().unwrap()).await?;
        assert_eq!(manifest.timeline_decisions.iter().find(|d| d.character_id == character_id).unwrap().selection, crate::timeline::Selection::Hold { frame:8 });
        assert_eq!(manifest.symbols["pet"].schedule.len(), 120);
        assert!(!manifest.states.is_empty());
        let mut pixel_hashes = BTreeSet::new();
        for state in manifest.states.values() {
            let svg = store.get("work", &state.svg_key).await?.unwrap();
            let tree = resvg::usvg::Tree::from_data(&svg,&resvg::usvg::Options::default())?;
            let mut pixmap = resvg::tiny_skia::Pixmap::new(256,256).unwrap();
            let scale = 256.0/tree.size().width().max(tree.size().height());
            resvg::render(&tree,resvg::tiny_skia::Transform::from_scale(scale,scale),&mut pixmap.as_mut());
            pixel_hashes.insert(crate::sha256(pixmap.data()));
            assert!(pixmap.data().chunks_exact(4).any(|p|p[3]>0), "normalized idle rendered transparent");
        }
        if class_name == "QuibXmasBank" {
            assert!(pixel_hashes.len() > 1, "Quibble child animation must remain visibly animated");
        }
        assert_eq!(export_source(&store,"work","source",&event,options()).await?["vector_cache_hit"], true);
        println!("{class_name}: 120 scheduled frames, {} unique SVGs; decisions {:?}; nonempty raster and cache hit verified", manifest.states.len(), manifest.timeline_decisions);
        Ok(())
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
