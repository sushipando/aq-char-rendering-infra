//! The exporter never imports the bounds module or rasterizes an image.
use anyhow::{ensure, Context, Result};
use aqw_component_raster::{
    import::{transformed_bounds, IDENTITY},
    svg::{self, FFDEC_NS, XLINK_NS},
};
use futures::{stream, StreamExt, TryStreamExt};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use tokio::process::Command;

use crate::{
    model::*,
    store::{self, Store},
    swf::Swf,
};

pub struct Ffdec {
    pub jar: PathBuf,
    pub deadline: Instant,
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

    pub async fn frames(
        &self,
        source: &Path,
        requests: &[SymbolRequest],
        destination: &Path,
        zoom: f64,
        start: usize,
        count: usize,
    ) -> Result<BTreeMap<String, Vec<Vec<u8>>>> {
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
            self.run(&destination.join("ffdec-home"), &args).await?;
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
    pub color_rules: BTreeMap<String, Vec<String>>,
    pub terminal_stops: BTreeMap<String, usize>,
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
        let class = Regex::new(r"\bclass\s+([A-Za-z_]\w*)\b")?;
        let Some(class) = class.captures(text) else {
            return Ok(());
        };
        let package = Regex::new(r"\bpackage(?:\s+([A-Za-z_][\w.]*))?\s*\{")?;
        let package = package
            .captures(text)
            .and_then(|c| c.get(1))
            .map(|m| m.as_str())
            .unwrap_or("");
        let name = if package.is_empty() {
            class[1].to_lowercase()
        } else {
            format!("{package}.{}", &class[1]).to_lowercase()
        };
        let color = Regex::new(
            r#"(?:mcSetColor|setColor)\s*\(\s*this\s*,\s*['"]([^'"]+)['"]\s*,\s*['"]([^'"]+)['"]\s*\)"#,
        )?;
        if let Some(color) = color.captures(text) {
            self.color_rules
                .insert(name.clone(), vec![color[1].into(), color[2].into()]);
        }
        let stop = Regex::new(r"(?:^|[^\w.])(?:this\.)?stop\s*\(\s*\)\s*;")?;
        for pair in Regex::new(r"(\d+)\s*,\s*this\.([A-Za-z_]\w*)")?.captures_iter(text) {
            let declaration = Regex::new(&format!(
                r"\bfunction\s+{}\s*\([^)]*\)[^{{]*\{{",
                regex::escape(&pair[2])
            ))?;
            if let Some(found) = declaration.find(text) {
                let mut depth = 1;
                for (index, c) in text[found.end()..].char_indices() {
                    if c == '{' {
                        depth += 1;
                    } else if c == '}' {
                        depth -= 1;
                    }
                    if depth == 0 {
                        if stop.is_match(&text[found.end()..found.end() + index]) {
                            let frame = pair[1].parse::<usize>()? + 1;
                            self.terminal_stops
                                .entry(name.clone())
                                .and_modify(|f| *f = (*f).min(frame))
                                .or_insert(frame);
                        }
                        break;
                    }
                }
            }
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

fn settled(
    request: &SymbolRequest,
    bytes: &[u8],
    stops: &BTreeMap<String, usize>,
) -> Result<Option<(SymbolRequest, Option<[f64; 6]>)>> {
    if stops.get(&request.class_name.to_lowercase()) == Some(&(request.frame + 1)) {
        let mut corrected = request.clone();
        corrected.frame += 1;
        corrected.root_timeline_frames = 1;
        return Ok(Some((corrected, None)));
    }
    let document = svg::parse(bytes)?;
    let rendered: Vec<_> = document
        .root
        .children
        .iter()
        .filter(|n| !matches!(n.local(), "defs" | "metadata" | "title" | "desc" | "style"))
        .collect();
    if rendered.len() != 1 || rendered[0].children.len() != 1 {
        return Ok(None);
    }
    let child = &rendered[0].children[0];
    if child.local() != "use"
        || ["filter", "clip-path", "mask", "opacity"]
            .iter()
            .any(|name| child.get(name).is_some())
    {
        return Ok(None);
    }
    let (Some(name), Some(id)) = (
        child.get_in("characterName", FFDEC_NS),
        child.get_in("characterId", FFDEC_NS),
    ) else {
        return Ok(None);
    };
    let stop = stops.get(&name.to_lowercase()).copied().unwrap_or(0);
    if stop < 2 {
        return Ok(None);
    }
    let placement = child
        .get("transform")
        .and_then(svg::parse_matrix)
        .unwrap_or(IDENTITY);
    Ok(Some((
        SymbolRequest {
            key: request.key.clone(),
            class_name: name.into(),
            character_id: id.parse()?,
            frame: stop,
            root_timeline_frames: 1,
        },
        Some(placement),
    )))
}

fn registration(bytes: &[u8], placement: [f64; 6], zoom: f64) -> Result<Vec<u8>> {
    let mut document = svg::parse(bytes)?;
    let root = &mut document.root;
    let length = |name: &str| -> Result<f64> {
        let raw = root.get(name).context("missing SVG dimension")?;
        Ok(raw.strip_suffix("px").unwrap_or(raw).parse()?)
    };
    let (width, height) = (length("width")?, length("height")?);
    let index = root
        .children
        .iter()
        .position(|n| n.local() == "g")
        .context("missing FFDec wrapper")?;
    let mut frame = root.children.remove(index);
    let m = frame
        .get("transform")
        .and_then(svg::parse_matrix)
        .context("missing FFDec registration matrix")?;
    ensure!(
        m[1].abs() < 1e-8
            && m[2].abs() < 1e-8
            && (m[0] - zoom).abs() < 1e-5
            && (m[3] - zoom).abs() < 1e-5,
        "unexpected FFDec registration matrix"
    );
    let [x, y, w, h] = transformed_bounds(
        [-m[4] / zoom, -m[5] / zoom, width / zoom, height / zoom],
        placement,
    );
    frame.set("transform", svg::matrix_text(placement));
    let mut wrapper = svg::Node::elem("g");
    wrapper.set(
        "transform",
        svg::matrix_text([zoom, 0.0, 0.0, zoom, -x * zoom, -y * zoom]),
    );
    wrapper.children.push(frame);
    root.children.insert(index, wrapper);
    root.set("width", format!("{}px", svg::fmt_g(w * zoom)));
    root.set("height", format!("{}px", svg::fmt_g(h * zoom)));
    Ok(svg::serialize(&document).into_bytes())
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

pub async fn export_source(
    store: &dyn Store,
    work_bucket: &str,
    source_bucket: &str,
    event: &Value,
    jar: PathBuf,
    timeout: Duration,
) -> Result<Value> {
    let started = Instant::now();
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
                && s["requests"] == source["requests"]),
        "source does not match prepare input"
    );
    let source_hash = source["sha256"]
        .as_str()
        .context("missing source checksum")?;
    let source_idx = source["idx"].as_u64().context("invalid source index")?;
    let vector_cache = prepared["cache"]["vectors"].as_bool().unwrap_or(true);
    let requests: Vec<SymbolRequest> = serde_json::from_value(source["requests"].clone())?;
    let settings = &prepared["settings"];
    let zoom = settings["zoom"].as_f64().context("missing zoom")?;
    let start = settings["subframe_start"]
        .as_u64()
        .context("missing subframe start")? as usize;
    let count = prepared["export_frame_count"]
        .as_u64()
        .context("missing export count")? as usize;
    let identity = crate::digest(
        &json!({"schema":VECTOR_SCHEMA,"policy":EXPORT_POLICY,"ffdec":FFDEC_VERSION,"sha256":source_hash,"requests":requests,"zoom":zoom,"start":start,"count":count}),
    )?;
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
                    json!({"job_id":job,"source_idx":source_idx,"cache_enabled":true,"cache_hit":true,"unique_states":cached.states.len(),"raster_probes":0,"duration_ms":started.elapsed().as_secs_f64()*1000.0}),
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
            computed.inspect(&String::from_utf8_lossy(&std::fs::read(path)?))?;
        }
        if vector_cache {
            store::write(store, work_bucket, &metadata_key, &computed, true).await?;
        }
        computed
    };
    let metadata_ms = started.elapsed().as_secs_f64() * 1000.0;
    let mut exported = ffdec
        .frames(
            &source_path,
            &requests,
            &temporary.path().join("exports"),
            zoom,
            start,
            count,
        )
        .await?;
    let mut corrections = BTreeMap::new();
    for request in &requests {
        if let Some(correction) = settled(
            request,
            &exported[&request.key][0],
            &metadata.terminal_stops,
        )? {
            corrections.insert(request.key.clone(), correction);
        }
    }
    if !corrections.is_empty() {
        let corrected: Vec<_> = corrections.values().map(|(r, _)| r.clone()).collect();
        let frames = ffdec
            .frames(
                &source_path,
                &corrected,
                &temporary.path().join("settled"),
                zoom,
                start,
                count,
            )
            .await?;
        for (key, frames) in frames {
            let effective = if let Some(placement) = corrections[&key].1 {
                frames
                    .iter()
                    .map(|bytes| registration(bytes, placement, zoom))
                    .collect::<Result<_>>()?
            } else {
                frames
            };
            exported.insert(key, effective);
        }
    }
    let ffdec_ms = started.elapsed().as_secs_f64() * 1000.0 - metadata_ms;
    let mut manifest = SourceManifest {
        schema_version: VECTOR_SCHEMA,
        export_policy: EXPORT_POLICY.into(),
        source_sha256: source_hash.into(),
        export_identity: identity,
        symbols: BTreeMap::new(),
        states: BTreeMap::new(),
        color_rules: metadata.color_rules,
        placement_colors: swf.placement_colors()?,
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
        let (effective, stop) = corrections
            .remove(&request.key)
            .map(|(r, _)| {
                let frame = r.frame;
                (r, Some(frame))
            })
            .unwrap_or((request.clone(), None));
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
    stream::iter(uploads)
        .map(|(key, bytes)| async move {
            store
                .put(work_bucket, &key, bytes, "image/svg+xml", true)
                .await
        })
        .buffer_unordered(16)
        .try_collect::<Vec<_>>()
        .await?;
    // Publish LAST: readers never observe a manifest whose SVGs aren't there.
    store::write(store, work_bucket, &manifest_key, &manifest, vector_cache).await?;
    crate::log(
        "prepare_export_complete",
        json!({"job_id":job,"source_idx":source_idx,"cache_enabled":vector_cache,"cache_hit":false,"exported_frames":manifest.symbols.values().map(|s|s.schedule.len()).sum::<usize>(),"unique_states":manifest.states.len(),"raster_probes":0,"metadata_ms":metadata_ms,"ffdec_ms":ffdec_ms,"duration_ms":started.elapsed().as_secs_f64()*1000.0}),
    );
    Ok(
        json!({"job_id":job,"source_idx":source_idx,"manifest_key":manifest_key,"vector_cache_hit":false}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn scripts_keep_cc_stops_and_random_pose() {
        let mut value = ScriptMetadata::default();
        value.inspect(r#"package aq { public class Test { addFrameScript(25,this.frame26); function frame26():void {this.stop();} mcSetColor(this,"Base","dark"); gotoAndStop(Math.round(Math.random()*totalFrames)); }}"#).unwrap();
        assert_eq!(value.terminal_stops["aq.test"], 26);
        assert_eq!(value.color_rules["aq.test"], vec!["Base", "dark"]);
        assert!(value.random_pose);
    }
    #[test]
    fn root_selection_ranges_are_deterministic() {
        assert_eq!(ranges(&[3, 1, 2, 3, 8, 9]), "1-3,8-9");
    }

    #[test]
    fn settled_root_and_child_keep_original_registration_without_probing() {
        let request = SymbolRequest {
            key: "pet".into(),
            class_name: "Pet".into(),
            character_id: 1,
            frame: 7,
            root_timeline_frames: 1,
        };
        let stops = BTreeMap::from([("pet".into(), 8)]);
        assert_eq!(
            settled(&request, b"not needed for adjacent root stop", &stops)
                .unwrap()
                .unwrap()
                .0
                .frame,
            8
        );
        let child = format!(
            r##"<svg xmlns="http://www.w3.org/2000/svg" xmlns:f="{FFDEC_NS}" width="100" height="80"><g transform="matrix(1 0 0 1 20 30)"><use f:characterName="Child" f:characterId="2" href="#shape" transform="matrix(-1 0 0 1 15 25)"/></g></svg>"##
        );
        let stops = BTreeMap::from([("child".into(), 26)]);
        let (corrected, placement) = settled(&request, child.as_bytes(), &stops)
            .unwrap()
            .unwrap();
        assert_eq!((corrected.character_id, corrected.frame), (2, 26));
        let original=br#"<svg xmlns="http://www.w3.org/2000/svg" width="100" height="80"><g transform="matrix(1 0 0 1 20 30)"><rect x="-20" y="-30" width="100" height="80"/></g></svg>"#;
        let effective = registration(original, placement.unwrap(), 1.0).unwrap();
        assert_ne!(crate::sha256(original), crate::sha256(&effective));
        let document = svg::parse(&effective).unwrap();
        let wrapper = &document.root.children[0];
        assert_eq!(
            svg::parse_matrix(wrapper.get("transform").unwrap()).unwrap(),
            [1.0, 0.0, 0.0, 1.0, 65.0, 5.0]
        );
        assert_eq!(
            svg::parse_matrix(wrapper.children[0].get("transform").unwrap()).unwrap(),
            [-1.0, 0.0, 0.0, 1.0, 15.0, 25.0]
        );
        let unsafe_child = child.replace("<use ", "<use opacity=\"0.5\" ");
        assert!(settled(&request, unsafe_child.as_bytes(), &stops)
            .unwrap()
            .is_none());
    }
}
