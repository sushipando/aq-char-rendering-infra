//! Resolve reachable literal MovieClip controls before FFDec generates any SVGs.
//! Controllers are held/loop-bounded in place; their animated children keep playing.
use crate::{
    model::SymbolRequest,
    script::{Action, Class, Command, Target},
    swf::{self, Swf},
};
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Selection {
    Hold {
        frame: usize,
    },
    Loop {
        first: usize,
        last: usize,
    },
    Sequence {
        frames: Vec<usize>,
        loop_start: Option<usize>,
    },
}
impl Selection {
    fn range(&self) -> (usize, usize) {
        match self {
            Self::Hold { frame } => (*frame, *frame),
            Self::Loop { first, last } => (*first, *last),
            Self::Sequence { frames, .. } => {
                (*frames.iter().min().unwrap(), *frames.iter().max().unwrap())
            }
        }
    }
    fn length(&self) -> usize {
        match self {
            Self::Sequence {
                frames,
                loop_start: Some(0),
            } => frames.len(),
            Self::Sequence { .. } => 12008,
            _ => {
                let (a, b) = self.range();
                b - a + 1
            }
        }
    }
    fn indices(&self) -> Vec<usize> {
        match self {
            Self::Sequence { frames, .. } => frames.clone(),
            _ => {
                let (a, b) = self.range();
                (a..=b).collect()
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Decision {
    pub character_id: u16,
    pub class_name: String,
    pub selection: Selection,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Instance {
    id: u16,
    generation: usize,
    name: Option<String>,
}
impl Instance {
    fn identity(&self) -> (u16, usize) {
        (self.id, self.generation)
    }
}
struct Frame {
    display: BTreeMap<u16, Instance>,
}
struct Clip {
    frames: Vec<Frame>,
    labels: BTreeMap<String, usize>,
}
impl Clip {
    fn read(payload: &[u8], budget: &mut usize, inert_actions: &BTreeSet<String>) -> Result<Self> {
        let mut frames = Vec::new();
        let mut labels = BTreeMap::new();
        let mut display: BTreeMap<u16, Instance> = BTreeMap::new();
        for (generation, (code, data)) in swf::tags(payload, 4)?.into_iter().enumerate() {
            *budget = budget
                .checked_sub(1)
                .context("timeline analysis work limit exceeded")?;
            if let Some((depth, id, name, moving)) = swf::instance(code, data)? {
                if let Some(id) = id {
                    let name = name.or_else(|| {
                        moving
                            .then(|| display.get(&depth).and_then(|i| i.name.clone()))
                            .flatten()
                    });
                    display.insert(
                        depth,
                        Instance {
                            id,
                            name,
                            generation,
                        },
                    );
                } else if let Some(name) = name {
                    display
                        .get_mut(&depth)
                        .context("rename of missing timeline instance")?
                        .name = Some(name);
                }
            }
            match code {
                1 => {
                    *budget = budget
                        .checked_sub(display.len())
                        .context("timeline display-list limit exceeded")?;
                    frames.push(Frame {
                        display: display.clone(),
                    });
                }
                5 => {
                    display.remove(&swf::u16_at(data, 2)?);
                }
                28 => {
                    display.remove(&swf::u16_at(data, 0)?);
                }
                43 => {
                    ensure!(
                        labels
                            .insert(swf::cstring(data, &mut 0)?, frames.len() + 1)
                            .is_none(),
                        "duplicate frame label"
                    );
                }
                // Only explicitly classified background setup can bypass AVM1 rejection.
                12 if inert_actions.contains(&crate::sha256(data)) => (),
                12 | 59 => bail!("AVM1 timeline actions are unsupported"),
                _ => (),
            }
        }
        ensure!(
            frames.len() == swf::u16_at(payload, 2)? as usize && !frames.is_empty(),
            "sprite frame count mismatch"
        );
        ensure!(
            labels.values().all(|f| *f <= frames.len()),
            "label beyond timeline"
        );
        Ok(Self { frames, labels })
    }
    fn target(&self, target: &Target, old_labels: bool) -> Result<usize> {
        let frame = match target {
            Target::Frame(frame) => (*frame).max(1).min(self.frames.len()),
            Target::Label(label) => {
                if let Ok(n) = label.parse::<usize>() {
                    n.max(1).min(self.frames.len())
                } else if let Some(frame) = self.labels.get(label) {
                    *frame
                } else if old_labels {
                    1
                } else {
                    bail!("unknown frame label {label:?}")
                }
            }
        };
        ensure!(
            (1..=self.frames.len()).contains(&frame),
            "timeline target out of range: {frame}"
        );
        Ok(frame)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Entry {
    target: Target,
    play: bool,
}

struct Resolver<'a> {
    swf: &'a Swf,
    scripts: &'a BTreeMap<String, Class>,
    inert_actions: &'a BTreeSet<String>,
    selections: BTreeMap<u16, Selection>,
    active: BTreeSet<u16>,
    // Same symbol used with incompatible per-instance state must not be globally rewritten.
    entries: BTreeMap<u16, Entry>,
    budget: usize,
    old_labels: bool,
    forced: BTreeMap<u16, Selection>,
    contexts: BTreeMap<u16, Vec<String>>,
    warnings: Vec<String>,
    graphic_instances: BTreeSet<u16>,
    child_values: BTreeMap<u16, BTreeMap<String, crate::script_eval::Value>>,
}
impl Resolver<'_> {
    fn visit(&mut self, id: u16, entry: Entry) -> Result<()> {
        if !self.swf.sprites.contains_key(&id) {
            return Ok(());
        }
        let name = self
            .swf
            .symbols
            .iter()
            .find(|(n, _)| *n == id)
            .map(|(_, s)| s.clone())
            .unwrap_or_default();
        self.visit_inner(id, entry)
            .with_context(|| format!("cannot resolve idle timeline: sprite {id} ({name})"))
    }
    fn visit_inner(&mut self, id: u16, entry: Entry) -> Result<()> {
        ensure!(!self.active.contains(&id), "cyclic sprite hierarchy");
        if let Some(previous) = self.entries.get(&id) {
            if *previous != entry {
                let clip =
                    Clip::read(&self.swf.sprites[&id], &mut self.budget, self.inert_actions)?;
                ensure!(
                    previous.play == entry.play
                        && clip.target(&previous.target, self.old_labels)?
                            == clip.target(&entry.target, self.old_labels)?,
                    "shared sprite has conflicting per-instance timeline states"
                );
            }
            return Ok(());
        }
        ensure!(
            self.active.len() < 64,
            "sprite hierarchy depth limit exceeded"
        );
        self.entries.insert(id, entry.clone());
        self.active.insert(id);
        let clip = Clip::read(&self.swf.sprites[&id], &mut self.budget, self.inert_actions)?;
        let name = self
            .swf
            .symbols
            .iter()
            .find(|(n, _)| *n == id)
            .map(|(_, s)| s.to_lowercase())
            .unwrap_or_default();
        let class = self.scripts.get(&name).cloned().unwrap_or_default();
        ensure!(
            class.unsupported.is_none() || class.runtime.is_some(),
            "unsupported script registration: {:?}",
            class.unsupported
        );
        let runtime_mode = class.runtime.is_some()
            && (class.unsupported.is_some()
                || class.constructor.unsupported.is_some()
                || class.frames.values().any(|p| p.unsupported.is_some()));
        ensure!(
            runtime_mode || class.constructor.unsupported.is_none(),
            "unsupported constructor: {:?}",
            class.constructor.unsupported
        );
        let mut frame = clip.target(&entry.target, self.old_labels)?;
        let mut _named_state = matches!(entry.target, Target::Label(_))
            || clip.labels.iter().any(|(label, at)| {
                *at == frame
                    && matches!(
                        label.to_ascii_lowercase().as_str(),
                        "idle" | "idel" | "id" | "ready"
                    )
            });
        let mut playing = entry.play;
        let mut runtime_state = crate::script_eval::State::default();
        let mut trace = Vec::new();
        let mut seen = BTreeMap::new();
        let mut child_entries: BTreeMap<(u16, usize), Entry> = BTreeMap::new();
        let selection;
        loop {
            self.budget = self
                .budget
                .checked_sub(1)
                .context("timeline execution work limit exceeded")?;
            let state_key = (frame, playing, serde_json::to_string(&runtime_state)?);
            if let Some(&start) = seen.get(&state_key) {
                ensure!(playing, "nonterminating stopped-frame script cycle");
                let cycle: &[usize] = &trace[start..];
                selection = if cycle.windows(2).all(|w| w[1] == w[0] + 1) {
                    Selection::Loop {
                        first: cycle[0],
                        last: *cycle.last().unwrap(),
                    }
                } else {
                    Selection::Sequence {
                        frames: cycle.to_vec(),
                        loop_start: Some(0),
                    }
                };
                break;
            }
            seen.insert(state_key, trace.len());
            trace.push(frame);
            let mut destination = frame;
            let mut commands = Vec::<Command>::new();
            if runtime_mode {
                let context = crate::script_eval::ContextData {
                    names: self.contexts.get(&id).cloned().unwrap_or_else(|| {
                        vec![
                            "asset".into(),
                            "holder".into(),
                            "mcChar".into(),
                            "stage".into(),
                        ]
                    }),
                    children: clip.frames[frame - 1]
                        .display
                        .values()
                        .filter_map(|i| i.name.clone())
                        .collect(),
                    foreign: BTreeMap::new(),
                    labels: Some(clip.labels.keys().cloned().collect()),
                    old_labels: self.old_labels,
                    frame,
                    total_frames: clip.frames.len(),
                    label: clip
                        .labels
                        .iter()
                        .filter(|(_, f)| **f <= frame)
                        .max_by_key(|(_, f)| *f)
                        .map(|(n, _)| n.clone()),
                };
                let mut eval = crate::script_eval::Evaluator::new(
                    class.runtime.as_ref().unwrap(),
                    &mut runtime_state,
                    &context,
                );
                eval.initialize()?;
                if trace.len() == 1 {
                    eval.event("Event.ADDED")?;
                    eval.event("Event.ADDED_TO_STAGE")?;
                }
                let mut initialized_commands = eval.commands;
                if trace.len() == 1 {
                    runtime_state
                        .values
                        .extend(self.child_values.remove(&id).unwrap_or_default());
                }
                let mut eval = crate::script_eval::Evaluator::new(
                    class.runtime.as_ref().unwrap(),
                    &mut runtime_state,
                    &context,
                );
                eval.commands.append(&mut initialized_commands);
                if trace.len() > 1 {
                    eval.event("Event.ENTER_FRAME")?;
                }
                eval.frame()?;
                commands.extend(eval.commands);
                for child in clip.frames[frame - 1].display.values() {
                    if let Some(name) = &child.name {
                        let prefix = format!("{name}.");
                        for (k, v) in &runtime_state.values {
                            if let Some(field) = k.strip_prefix(&prefix) {
                                self.child_values
                                    .entry(child.id)
                                    .or_default()
                                    .insert(field.into(), v.clone());
                            }
                        }
                    }
                }
                for fault in &runtime_state.faults {
                    if !self.warnings.contains(fault) {
                        self.warnings.push(fault.clone());
                    }
                }
                ensure!(
                    !runtime_state
                        .values
                        .keys()
                        .any(|k| k == "visible" || k.ends_with(".visible")),
                    "runtime clip needs visibility normalization"
                );
            } else {
                if trace.len() == 1 {
                    commands.extend(class.constructor.commands.clone());
                }
                if let Some(program) = class.frames.get(&frame) {
                    ensure!(
                        program.unsupported.is_none(),
                        "unsupported control on reachable frame {frame}: {:?}",
                        program.unsupported
                    );
                    commands.extend(program.commands.clone());
                }
            }
            for command in commands {
                if let Some(child) = command.child {
                    let matches: Vec<_> = clip.frames[frame - 1]
                        .display
                        .values()
                        .filter(|i| i.name.as_deref() == Some(&child))
                        .collect();
                    ensure!(
                        matches.len() == 1,
                        "frame {frame}: child {child:?} is missing or ambiguous"
                    );
                    let entry = match command.action {
                        Action::Goto { target, play } => Entry { target, play },
                        Action::Stop | Action::Play => {
                            bail!("child stop/play needs the synchronized instance clock")
                        }
                        Action::FirstRandomPose => Entry {
                            target: Target::Frame(1),
                            play: false,
                        },
                    };
                    child_entries.insert(matches[0].identity(), entry);
                } else {
                    match command.action {
                        Action::Stop => playing = false,
                        Action::Play => playing = true,
                        Action::FirstRandomPose => {
                            destination = 1;
                            playing = false;
                        }
                        Action::Goto { target, play } => {
                            _named_state |= matches!(target, Target::Label(_));
                            destination = match clip.target(&target, self.old_labels) {
                                Ok(f) => f,
                                Err(e) if e.to_string().starts_with("unknown frame label") => {
                                    let warning = format!("{name}: {e}");
                                    if !self.warnings.contains(&warning) {
                                        self.warnings.push(warning);
                                    }
                                    break;
                                }
                                Err(e) => return Err(e),
                            };
                            playing = play;
                        }
                    }
                }
            }
            if !playing {
                // A literal gotoAndStop selects its destination, whose callback also runs.
                if destination != frame {
                    frame = destination;
                    continue;
                }
                selection = Selection::Hold { frame };
                break;
            }
            let next = if destination != frame {
                destination
            } else {
                frame % clip.frames.len() + 1
            };
            // Labels do not stop Flash playback. Follow authored control flow.
            frame = next;
        }
        let selection = if let Some(forced) = self.forced.remove(&id) {
            ensure!(
                !runtime_mode
                    && class
                        .frames
                        .values()
                        .chain([&class.constructor])
                        .all(|p| p.commands.is_empty()),
                "generated Graphic controls a child with custom timeline actions"
            );
            forced
        } else {
            selection
        };
        let mut children = BTreeSet::new();
        for frame in selection.indices() {
            children.extend(clip.frames[frame - 1].display.values().cloned());
        }
        // A child command during a repeating parent would reset its playhead each cycle.
        // Our independent-child export cannot represent that synchronization safely.
        if matches!(
            selection,
            Selection::Loop { .. } | Selection::Sequence { .. }
        ) {
            ensure!(
                child_entries.is_empty(),
                "repeating parent controls child playheads; synchronized timelines are unsupported"
            );
        }
        for child in children {
            if child
                .name
                .as_ref()
                .is_some_and(|name| class.graphic_children.contains(name))
            {
                self.graphic_instances.insert(child.id);
            }
            if child
                .name
                .as_ref()
                .is_some_and(|name| class.synchronized_children.contains(name))
            {
                let count = swf::u16_at(&self.swf.sprites[&child.id], 2)? as usize;
                ensure!(count > 0, "empty synchronized layer");
                let indices: Vec<_> = selection
                    .indices()
                    .into_iter()
                    .map(|f| {
                        if self.graphic_instances.contains(&id) {
                            (f - 1) % count + 1
                        } else {
                            f.min(count)
                        }
                    })
                    .collect();
                let forced = if indices.iter().all(|f| *f == indices[0]) {
                    Selection::Hold { frame: indices[0] }
                } else if indices.windows(2).all(|w| w[1] == w[0] + 1) {
                    Selection::Loop {
                        first: indices[0],
                        last: *indices.last().unwrap(),
                    }
                } else {
                    Selection::Sequence {
                        frames: indices,
                        loop_start: Some(0),
                    }
                };
                self.forced.insert(child.id, forced);
            }
            let entry = child_entries
                .get(&child.identity())
                .cloned()
                .unwrap_or(Entry {
                    target: Target::Frame(1),
                    play: true,
                });
            self.visit(child.id, entry)?;
        }
        self.selections.insert(id, selection);
        self.active.remove(&id);
        Ok(())
    }
}

fn rewrite(payload: &[u8], selection: &Selection) -> Result<Vec<u8>> {
    if let Selection::Sequence { frames, loop_start } = selection {
        let mut indices = frames.clone();
        while indices.len() < selection.length() {
            indices.push(if let Some(start) = loop_start {
                frames[start + (indices.len() - start) % (frames.len() - start)]
            } else {
                *frames.last().unwrap()
            });
        }
        return crate::display::sequence(payload, &indices, 0);
    }
    let (first, last) = selection.range();
    let mut output = Vec::from(&payload[..2]);
    output.extend_from_slice(&u16::try_from(last - first + 1)?.to_le_bytes());
    let mut frame = 1;
    for (code, data) in swf::tags(payload, 4)? {
        if frame > last {
            break;
        }
        match code {
            0 | 43 => (), // Export-only timeline: frame labels no longer describe original positions.
            1 => {
                if frame >= first {
                    swf::write_tag(&mut output, code, data);
                }
                frame += 1;
            }
            _ => swf::write_tag(&mut output, code, data),
        }
    }
    swf::write_tag(&mut output, 0, &[]);
    Ok(output)
}

pub struct Normalized {
    pub bytes: Vec<u8>,
    pub requests: Vec<SymbolRequest>,
    pub decisions: Vec<Decision>,
    pub symbol_aliases: BTreeMap<String, String>,
    pub host_visibility: BTreeMap<String, bool>,
    pub warnings: Vec<String>,
}

pub fn normalize(
    source: &[u8],
    swf: &Swf,
    scripts: &BTreeMap<String, Class>,
    requests: &[SymbolRequest],
) -> Result<Normalized> {
    normalize_with_avm1(source, swf, scripts, requests, &BTreeSet::new())
}

pub(crate) fn normalize_with_avm1(
    source: &[u8],
    swf: &Swf,
    scripts: &BTreeMap<String, Class>,
    requests: &[SymbolRequest],
    inert_actions: &BTreeSet<String>,
) -> Result<Normalized> {
    let repaired = swf::repair_zero_frame_displays(source, swf)?;
    let parsed = repaired.as_ref().map(|b| Swf::parse(b)).transpose()?;
    let (source, swf) = if let (Some(b), Some(s)) = (&repaired, &parsed) {
        (b.as_slice(), s)
    } else {
        (source, swf)
    };
    if requests.iter().any(|r| r.click)
        && scripts
            .values()
            .any(|c| c.runtime.as_ref().is_some_and(|r| r.has_click_listener()))
    {
        ensure!(
            inert_actions.is_empty(),
            "animation clicks require AS3 timeline metadata"
        );
        return crate::scene_timeline::normalize(source, swf, scripts, requests);
    }
    match normalize_static(source, swf, scripts, requests, inert_actions) {
        Ok(v) => Ok(v),
        Err(e) => {
            let message = format!("{e:#}");
            if inert_actions.is_empty()
                && (message.contains("repeating parent controls")
                    || message.contains("child \"parent\"")
                    || message.contains("child stop/play")
                    || message.contains("visibility normalization")
                    || message.contains("generated Graphic controls"))
            {
                crate::scene_timeline::normalize(source, swf, scripts, requests)
                    .with_context(|| format!("synchronized fallback after {message}"))
            } else {
                Err(e)
            }
        }
    }
}

fn normalize_static(
    source: &[u8],
    swf: &Swf,
    scripts: &BTreeMap<String, Class>,
    requests: &[SymbolRequest],
    inert_actions: &BTreeSet<String>,
) -> Result<Normalized> {
    let needs_context = requests.len() > 1
        || requests.iter().any(|r| r.click)
        || scripts.values().any(|c| {
            !c.synchronized_children.is_empty()
                || c.constructor.unsupported.is_some()
                || c.frames.values().chain([&c.constructor]).any(|p| {
                    p.unsupported.is_some() || p.commands.iter().any(|cmd| cmd.child.is_some())
                })
        });
    let isolated = if needs_context {
        Some(crate::instances::isolate(source, swf, scripts, requests)?)
    } else {
        None
    };
    let (source, swf, scripts, requests) = if let Some(i) = &isolated {
        (
            i.bytes.as_slice(),
            &i.swf,
            &i.scripts,
            i.requests.as_slice(),
        )
    } else {
        (source, swf, scripts, requests)
    };
    let mut resolver = Resolver {
        swf,
        scripts,
        inert_actions,
        selections: BTreeMap::new(),
        active: BTreeSet::new(),
        entries: BTreeMap::new(),
        budget: 2_000_000,
        old_labels: source[3] < 11,
        forced: BTreeMap::new(),
        contexts: isolated
            .as_ref()
            .map(|i| i.names.clone())
            .unwrap_or_default(),
        warnings: Vec::new(),
        graphic_instances: BTreeSet::new(),
        child_values: BTreeMap::new(),
    };
    for request in requests {
        ensure!(
            swf.sprites.contains_key(&request.character_id),
            "requested root is not a sprite"
        );
        // The root is sampled at the requested state; script-free roots retain the
        // exporter's existing static-vs-animated selection. Scripted roots settle.
        let class = scripts.get(&request.class_name.to_lowercase());
        let scripted_self = class.is_some_and(|c| {
            c.frames.values().chain([&c.constructor]).any(|p| {
                p.unsupported.is_some() || p.commands.iter().any(|cmd| cmd.child.is_none())
            })
        });
        resolver.visit(
            request.character_id,
            Entry {
                target: Target::Frame(request.frame),
                play: request.root_timeline_frames > 1 || scripted_self,
            },
        )?;
    }
    let mut replacements = BTreeMap::new();
    let mut decisions = Vec::new();
    for (id, selection) in &resolver.selections {
        let count = swf::u16_at(&swf.sprites[id], 2)? as usize;
        if *selection
            == (Selection::Loop {
                first: 1,
                last: count,
            })
            || (count == 1 && *selection == (Selection::Hold { frame: 1 }))
        {
            continue;
        }
        replacements.insert(*id, rewrite(&swf.sprites[id], selection)?);
        decisions.push(Decision {
            character_id: *id,
            class_name: swf
                .symbols
                .iter()
                .find(|(n, _)| n == id)
                .map(|(_, s)| s.clone())
                .unwrap_or_default(),
            selection: selection.clone(),
        });
    }
    let mut effective = requests.to_vec();
    for request in &mut effective {
        if replacements.contains_key(&request.character_id) {
            let selection = &resolver.selections[&request.character_id];
            request.frame = 1;
            request.root_timeline_frames = selection.length();
        }
    }
    Ok(Normalized {
        bytes: swf::replace_sprites(source, &replacements)?,
        requests: effective,
        decisions,
        symbol_aliases: isolated
            .as_ref()
            .map(|i| i.aliases.clone())
            .unwrap_or_default(),
        host_visibility: BTreeMap::new(),
        warnings: resolver.warnings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_sprite_uses_each_actual_ancestor_name() {
        let (bytes, swf) = fixture(vec![(1, "Root", vec![vec![], vec![], vec![]])]);
        let classes = scripts(
            r#"class Root {function Root(){addFrameScript(0,frame1);}function frame1(){if(parent.name=="backshoulder"){gotoAndStop(3);}else{stop();}}}"#,
        );
        let mut front = request();
        front.ancestor_names = vec!["frontshoulder".into(), "mcChar".into()];
        let mut back = front.clone();
        back.key = "back".into();
        back.ancestor_names[0] = "backshoulder".into();
        let result = normalize(&bytes, &swf, &classes, &[front, back]).unwrap();
        assert_ne!(
            result.requests[0].character_id,
            result.requests[1].character_id
        );
        assert!(result
            .decisions
            .iter()
            .any(|d| d.selection == Selection::Hold { frame: 1 }));
        assert!(result
            .decisions
            .iter()
            .any(|d| d.selection == Selection::Hold { frame: 3 }));
    }
    #[test]
    fn click_occurs_once_after_initialization_and_keeps_final_pose() {
        let (bytes, swf) = fixture(vec![
            (1, "Root", vec![place(2, 1, "art"), vec![], vec![]]),
            (2, "Art", vec![vec![]]),
        ]);
        let classes=scripts("class Root {public var inits:int=0;public var clicks:int=0;function Root(){inits++;addFrameScript(0,frame1,2,frame3);}function frame1(){addEventListener(MouseEvent.CLICK,onClick);stop();}function onClick(e:Object){clicks++;gotoAndPlay(inits+clicks);}function frame3(){stop();}}");
        let mut req = request();
        req.click = true;
        req.capture_end = 8;
        let result = normalize(&bytes, &swf, &classes, &[req]).unwrap();
        let decision = result
            .decisions
            .iter()
            .find(|d| d.character_id == 1)
            .unwrap();
        assert_eq!(
            decision.selection,
            Selection::Sequence {
                frames: vec![2, 3],
                loop_start: Some(1)
            }
        );
        let parsed = Swf::parse(&result.bytes).unwrap();
        let frames = crate::display::snapshots(&parsed.sprites[&1]).unwrap();
        // A static parent placement can be collapsed while its child continues.
        assert!(!frames[0].is_empty());
    }
    #[test]
    fn selecting_later_idle_keeps_initial_click_registration() {
        let (bytes, swf) = fixture(vec![(1, "Root", vec![vec![], vec![], vec![]])]);
        let classes=scripts("class Root {function Root(){addFrameScript(0,frame1,1,frame2);}function frame1(){addEventListener(MouseEvent.CLICK,onClick);}function frame2(){stop();}function onClick(e:Object){gotoAndStop(3);}}");
        let mut req = request();
        req.frame = 2;
        req.click = true;
        req.capture_end = 4;
        let result = normalize(&bytes, &swf, &classes, &[req]).unwrap();
        assert_eq!(result.decisions[0].selection, Selection::Hold { frame: 3 });
    }
    #[test]
    fn graphic_loop_wraps_shorter_art_on_the_parent_clock() {
        let (bytes, swf) = fixture(vec![
            (
                1,
                "Root",
                vec![
                    place(2, 1, "wrapper"),
                    vec![],
                    vec![],
                    vec![],
                    vec![],
                    vec![],
                ],
            ),
            (
                2,
                "Wrapper",
                vec![place(3, 1, "art"), vec![], vec![], vec![], vec![], vec![]],
            ),
            (3, "Art", vec![vec![], vec![]]),
        ]);
        let classes = BTreeMap::from([
            (
                "root".into(),
                Class {
                    synchronized_children: vec!["wrapper".into()],
                    graphic_children: vec!["wrapper".into()],
                    ..Class::default()
                },
            ),
            (
                "wrapper".into(),
                Class {
                    synchronized_children: vec!["art".into()],
                    ..Class::default()
                },
            ),
        ]);
        let mut req = request();
        req.root_timeline_frames = 6;
        let result = normalize(&bytes, &swf, &classes, &[req]).unwrap();
        let art = result
            .decisions
            .iter()
            .find(|d| d.character_id == 3)
            .unwrap();
        assert_eq!(art.selection.indices(), vec![1, 2, 1, 2, 1, 2]);
    }
    #[test]
    fn ui_only_click_does_not_change_timeline() {
        let (bytes, swf) = fixture(vec![(1, "Root", vec![vec![], vec![]])]);
        let classes=scripts("class Root {function Root(){addFrameScript(0,frame1);}function frame1(){stop();addEventListener(MouseEvent.CLICK,shop);}function shop(e:Object){MovieClip(stage.getChildAt(0)).world.openShop(12);}}");
        let mut req = request();
        req.click = true;
        req.capture_end = 4;
        let result = normalize(&bytes, &swf, &classes, &[req]).unwrap();
        assert_eq!(result.decisions[0].selection, Selection::Hold { frame: 1 });
    }
    #[test]
    fn avm1_approval_is_bound_to_action_bytes_and_tag_kind() {
        let action = b"recognized background action fixture";
        let allowed = BTreeSet::from([crate::sha256(action)]);
        let clip = |code, data: &[u8]| {
            let mut payload = vec![1,0,1,0];
            payload.extend(tag(code,data));
            payload.extend(tag(1,&[]));
            payload.extend(tag(0,&[]));
            payload
        };
        assert!(Clip::read(&clip(12,action), &mut 1000, &allowed).is_ok());
        assert!(Clip::read(&clip(12,action), &mut 1000, &BTreeSet::new()).is_err());
        assert!(Clip::read(&clip(12,b"other action"), &mut 1000, &allowed).is_err());
        assert!(Clip::read(&clip(59,action), &mut 1000, &allowed).is_err());
    }

    fn tag(code: u16, data: &[u8]) -> Vec<u8> {
        let mut b = Vec::new();
        swf::write_tag(&mut b, code, data);
        b
    }
    fn place(id: u16, depth: u16, name: &str) -> Vec<u8> {
        let mut b = vec![0x26];
        b.extend(depth.to_le_bytes());
        b.extend(id.to_le_bytes());
        b.push(0);
        b.extend(name.as_bytes());
        b.push(0);
        tag(26, &b)
    }
    fn sprite(id: u16, frames: Vec<Vec<u8>>) -> Vec<u8> {
        let mut b = Vec::from(id.to_le_bytes());
        b.extend((frames.len() as u16).to_le_bytes());
        for f in frames {
            b.extend(f);
            b.extend(tag(1, &[]));
        }
        b.extend(tag(0, &[]));
        b
    }
    fn fixture(parts: Vec<(u16, &str, Vec<Vec<u8>>)>) -> (Vec<u8>, Swf) {
        let mut body = vec![0, 0, 24, 1, 0];
        let mut names = Vec::from((parts.len() as u16).to_le_bytes());
        for (id, name, frames) in parts {
            body.extend(tag(39, &sprite(id, frames)));
            names.extend(id.to_le_bytes());
            names.extend(name.as_bytes());
            names.push(0);
        }
        body.extend(tag(76, &names));
        body.extend(tag(1, &[]));
        body.extend(tag(0, &[]));
        let mut b = Vec::from(b"FWS\x0a");
        b.extend(((body.len() + 8) as u32).to_le_bytes());
        b.extend(body);
        let parsed = Swf::parse(&b).unwrap();
        (b, parsed)
    }
    fn scripts(text: &str) -> BTreeMap<String, Class> {
        BTreeMap::from([crate::script::parse(text).unwrap().unwrap()])
    }
    fn request() -> SymbolRequest {
        SymbolRequest {
            key: "pet".into(),
            class_name: "Root".into(),
            character_id: 1,
            frame: 1,
            root_timeline_frames: 1,
            click: false,
            ancestor_names: Vec::new(),
            capture_end: 2008,
        }
    }

    #[test]
    fn bank_child_idle_selects_idle_art_and_keeps_animated_descendants() {
        let (bytes, swf) = fixture(vec![
            (1, "Root", vec![place(2, 1, "CCPet"), vec![], vec![]]),
            (
                2,
                "Child",
                vec![
                    vec![],
                    [tag(43, b"Idle\0"), place(3, 1, "idle")].concat(),
                    [tag(43, b"Walk\0"), place(4, 1, "walk")].concat(),
                ],
            ),
            (3, "Flame", vec![vec![], vec![], vec![]]),
            (4, "Walking", vec![vec![]]),
        ]);
        let mut scripts=scripts("class Root {function Root(){addFrameScript(0,this.frame1);}function initPet(){}function frame1(){this.CCPet.gotoAndPlay(\"Idle\");if(!petInit){petInit=true;initPet();}stop();}}");
        let child = crate::script::parse(
            "class Child {function Child(){addFrameScript(1,this.idle);}function idle(){stop();}}",
        )
        .unwrap()
        .unwrap();
        scripts.insert(child.0, child.1);
        let normalized = normalize(&bytes, &swf, &scripts, &[request()]).unwrap();
        let updated = Swf::parse(&normalized.bytes).unwrap();
        assert!(normalized
            .decisions
            .iter()
            .any(|d| d.character_id == 2 && d.selection == Selection::Hold { frame: 2 }));
        assert_eq!(
            updated.sprites[&3], swf.sprites[&3],
            "nested idle animation must be untouched"
        );
        let child = Clip::read(&updated.sprites[&2], &mut 1000, &BTreeSet::new()).unwrap();
        assert_eq!(
            child.frames[0].display[&1].id, 3,
            "walking art must not replace idle art"
        );
    }

    #[test]
    fn randomized_start_policy_preserves_full_child_loop() {
        let (bytes, swf) = fixture(vec![
            (1, "Root", vec![place(2, 1, "flame")]),
            (2, "Flame", vec![vec![], vec![], vec![]]),
        ]);
        let scripts=scripts("class Flame extends MovieClip {public var started:*;public function Flame(){super();addFrameScript(0,this.frame1);}internal function frame1():*{if(this.started==undefined){this.started=true;gotoAndPlay(Math.ceil(Math.random()*totalFrames));}}}");
        let normalized = normalize(&bytes, &swf, &scripts, &[request()]).unwrap();
        assert_eq!(
            normalized.bytes, bytes,
            "the three-frame effect must not freeze or lose frames"
        );
        assert!(normalized.decisions.is_empty());
    }

    #[test]
    fn recursive_controllers_preserve_siblings_removals_and_animated_artwork() {
        let (bytes, swf) = fixture(vec![
            (
                1,
                "Root",
                vec![
                    [place(2, 1, "pet"), place(4, 2, "sibling")].concat(),
                    tag(28, &[2, 0]),
                ],
            ),
            (
                2,
                "Child",
                vec![
                    [tag(43, b"Idle\0"), place(3, 1, "art")].concat(),
                    vec![],
                    [tag(43, b"Walk\0"), place(5, 1, "walk")].concat(),
                ],
            ),
            (
                3,
                "Art",
                vec![vec![], tag(43, b"ordinary_frame_label\0"), vec![]],
            ),
            (4, "Sibling", vec![vec![]]),
            (5, "Walk", vec![vec![]]),
        ]);
        let mut metadata = scripts(
            r#"class Root { function Root() {addFrameScript(0,this.a);} function a() {this.pet.gotoAndPlay("Idle"); stop();} }"#,
        );
        metadata.extend(scripts(r#"class Child { function Child(){addFrameScript(1,this.hold,2,this.walk);} function hold(){stop();} function walk(){if(moving) gotoAndPlay("Walk");} }"#));
        let fixed = normalize(&bytes, &swf, &metadata, &[request()]).unwrap();
        let updated = Swf::parse(&fixed.bytes).unwrap();
        assert_eq!(
            updated.sprites[&3], swf.sprites[&3],
            "artwork must remain animated and byte-identical"
        );
        assert_eq!(
            fixed
                .decisions
                .iter()
                .find(|d| d.character_id == 2)
                .unwrap()
                .selection,
            Selection::Hold { frame: 2 }
        );
        let child = Clip::read(&updated.sprites[&2], &mut 1000, &BTreeSet::new()).unwrap();
        assert_eq!(child.frames.len(), 1);
        assert_eq!(child.frames[0].display[&1].id, 3);
        let root = Clip::read(&updated.sprites[&1], &mut 1000, &BTreeSet::new()).unwrap();
        assert_eq!(
            root.frames[0].display.len(),
            2,
            "sibling must not be discarded"
        );
        assert_eq!(
            fixed.requests[0].character_id, 1,
            "retain the root registration and hierarchy"
        );
    }

    #[test]
    fn literal_loop_excludes_other_states_and_dynamic_reachable_control_errors() {
        let (bytes, swf) = fixture(vec![(
            1,
            "Root",
            vec![tag(43, b"Idle\0"), vec![], tag(43, b"Walk\0")],
        )]);
        let metadata = scripts(
            r#"class Root {function Root(){addFrameScript(1,this.back);} function back(){gotoAndPlay("Idle");}}"#,
        );
        let fixed = normalize(&bytes, &swf, &metadata, &[request()]).unwrap();
        assert_eq!(
            fixed.decisions[0].selection,
            Selection::Loop { first: 1, last: 2 }
        );
        assert_eq!(fixed.requests[0].root_timeline_frames, 2);
        let metadata = scripts(
            "class Root {function Root(){addFrameScript(0,this.f);} function f(){if(x) stop();}}",
        );
        let error = normalize(&bytes, &swf, &metadata, &[request()])
            .err()
            .unwrap();
        assert!(format!("{error:#}").contains("unknown identifier x"));
    }

    #[test]
    fn modern_missing_labels_are_rejected() {
        let (mut bytes, swf) = fixture(vec![
            (
                1,
                "Root",
                vec![[place(2, 1, "a"), place(2, 2, "b")].concat()],
            ),
            (2, "Child", vec![tag(43, b"Idle\0"), tag(43, b"Walk\0")]),
        ]);
        bytes[3] = 11;
        for body in ["this.a.gotoAndStop(\"Missing\");stop();"] {
            let metadata = scripts(&format!("class Root {{function Root(){{addFrameScript(0,this.f);}} function f(){{{body}}}}}"));
            assert!(
                normalize(&bytes, &swf, &metadata, &[request()]).is_err(),
                "{body}"
            );
        }
    }

    #[test]
    fn preserved_raw_tags_include_matrix_color_mask_filter_and_removal_updates() {
        // PlaceObject3 with matrix, CXFORM, name, clip depth, empty filter list;
        // then a MOVE-only update and sibling removal. No reconstruction of fields.
        let mut placement = vec![0x6e, 1, 1, 0, 2, 0, 0, 0];
        placement.extend(b"child\0");
        placement.extend([3, 0, 0]);
        let first = [tag(70, &placement), place(3, 2, "removed")].concat();
        let second = [tag(26, &[5, 1, 0, 0]), tag(28, &[2, 0])].concat();
        let (bytes, swf) = fixture(vec![
            (1, "Root", vec![first, second]),
            (2, "Art", vec![vec![], vec![]]),
            (3, "Other", vec![vec![]]),
        ]);
        let metadata = scripts(
            "class Root {function Root(){addFrameScript(1,this.f);} function f(){stop();}}",
        );
        let fixed = normalize(&bytes, &swf, &metadata, &[request()]).unwrap();
        let updated = Swf::parse(&fixed.bytes).unwrap();
        let controls = |b: &[u8]| {
            swf::tags(b, 4)
                .unwrap()
                .into_iter()
                .filter(|(c, _)| !matches!(c, 0 | 1 | 43))
                .map(|(c, b)| (c, b.to_vec()))
                .collect::<Vec<_>>()
        };
        assert_eq!(controls(&swf.sprites[&1]), controls(&updated.sprites[&1]));
        assert_eq!(
            Clip::read(&updated.sprites[&1], &mut 1000, &BTreeSet::new()).unwrap().frames[0]
                .display
                .len(),
            1
        );
    }

    #[test]
    fn plain_root_selection_and_plain_child_animation_are_preserved() {
        let (bytes, swf) = fixture(vec![
            (
                1,
                "Root",
                vec![
                    tag(43, b"Ready\0"),
                    [tag(43, b"Idle\0"), place(2, 1, "art")].concat(),
                    tag(43, b"Walk\0"),
                ],
            ),
            (2, "Art", vec![vec![], vec![]]),
        ]);
        let mut r = request();
        r.frame = 2;
        let fixed = normalize(&bytes, &swf, &BTreeMap::new(), &[r]).unwrap();
        assert_eq!(fixed.decisions[0].selection, Selection::Hold { frame: 2 });
        assert_eq!(
            Swf::parse(&fixed.bytes).unwrap().sprites[&2],
            swf.sprites[&2]
        );
    }

    #[test]
    fn compressed_input_is_equivalent_and_hierarchy_cycles_fail() {
        use std::io::Write;
        let (bytes, swf) = fixture(vec![(1, "Root", vec![vec![], vec![]])]);
        let mut encoder =
            flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(&bytes[8..]).unwrap();
        let mut compressed = bytes[..8].to_vec();
        compressed[0] = b'C';
        compressed.extend(encoder.finish().unwrap());
        let a = normalize(&bytes, &swf, &BTreeMap::new(), &[request()]).unwrap();
        let b = normalize(
            &compressed,
            &Swf::parse(&compressed).unwrap(),
            &BTreeMap::new(),
            &[request()],
        )
        .unwrap();
        assert_eq!(a.bytes, b.bytes);
        let (bytes, swf) = fixture(vec![(1, "Root", vec![place(1, 1, "recursive")])]);
        assert!(format!(
            "{:#}",
            normalize(&bytes, &swf, &BTreeMap::new(), &[request()])
                .err()
                .unwrap()
        )
        .contains("cyclic sprite hierarchy"));
    }

    #[test]
    fn goto_destination_callbacks_and_last_command_order_are_respected() {
        let (bytes, swf) = fixture(vec![(1, "Root", vec![vec![], vec![], vec![]])]);
        let metadata = scripts("class Root {function Root(){addFrameScript(0,this.a,1,this.b,2,this.c);} function a(){stop();gotoAndPlay(2);} function b(){gotoAndStop(3);} function c(){stop();}}");
        assert_eq!(
            normalize(&bytes, &swf, &metadata, &[request()])
                .unwrap()
                .decisions[0]
                .selection,
            Selection::Hold { frame: 3 }
        );
    }

    #[test]
    fn arbitrary_named_child_states_are_bounded_and_deep_controllers_settle() {
        let (bytes, swf) = fixture(vec![
            (1, "Root", vec![place(2, 1, "pet")]),
            (
                2,
                "Child",
                vec![tag(43, b"Resting\0"), tag(43, b"Travel\0")],
            ),
        ]);
        let metadata = scripts(
            r#"class Root {function Root(){addFrameScript(0,this.f);} function f(){this.pet.gotoAndPlay("Resting");stop();}}"#,
        );
        assert!(
            normalize(&bytes, &swf, &metadata, &[request()]).is_ok(),
            "labels do not stop playback"
        );
        let (bytes, swf) = fixture(vec![
            (1, "Root", vec![place(2, 1, "child"), vec![]]),
            (2, "C2", vec![place(3, 1, "child"), vec![]]),
            (3, "C3", vec![place(4, 1, "child"), vec![]]),
            (4, "C4", vec![place(5, 1, "child"), vec![]]),
            (5, "Art", vec![vec![], vec![], vec![]]),
        ]);
        let mut metadata = BTreeMap::new();
        for name in ["Root", "C2", "C3", "C4"] {
            metadata.extend(scripts(&format!("class {name} {{function {name}(){{addFrameScript(1,this.hold);}} function hold(){{stop();}}}}")));
        }
        let fixed = normalize(&bytes, &swf, &metadata, &[request()]).unwrap();
        assert_eq!(fixed.decisions.len(), 4);
        assert!(fixed
            .decisions
            .iter()
            .all(|d| d.selection == (Selection::Hold { frame: 2 })));
        assert_eq!(
            Swf::parse(&fixed.bytes).unwrap().sprites[&5],
            swf.sprites[&5]
        );
    }
}
