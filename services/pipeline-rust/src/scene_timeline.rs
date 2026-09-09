//! Synchronized timeline fallback. Advance an actual instance tree on a shared
//! clock, then bake lossless display-list deltas for FFDec's non-scripted export.
use crate::{
    display::{self, Display},
    instances,
    model::SymbolRequest,
    script::{Action, Class, Command, Target},
    script_eval::{ContextData, Evaluator, State, Value},
    swf::{self, Swf},
    timeline::{Decision, Normalized, Selection},
};
use anyhow::{bail, ensure, Context, Result};
use std::collections::{BTreeMap, BTreeSet};
struct Clip {
    frames: Vec<Display>,
    labels: BTreeMap<String, usize>,
    class: Class,
}
struct Node {
    parent: Option<u16>,
    frame: usize,
    playing: bool,
    state: State,
    age: usize,
    record: Vec<Display>,
    original_frames: Vec<usize>,
    active: bool,
    constructed: bool,
    ran: bool,
}
struct Scene<'a> {
    swf: &'a Swf,
    clips: BTreeMap<u16, Clip>,
    nodes: BTreeMap<u16, Node>,
    names: &'a BTreeMap<u16, Vec<String>>,
    roots: Vec<u16>,
    budget: usize,
    depth: usize,
    old_labels: bool,
    host_visibility: BTreeMap<String, bool>,
    warnings: Vec<String>,
}
impl Scene<'_> {
    fn work(&mut self) -> Result<()> {
        self.budget = self
            .budget
            .checked_sub(1)
            .context("synchronized timeline work limit exceeded")?;
        Ok(())
    }
    fn target(&self, id: u16, target: &Target) -> Result<usize> {
        let clip = &self.clips[&id];
        Ok(match target {
            Target::Frame(n) => (*n).max(1).min(clip.frames.len()),
            Target::Label(s) => {
                if let Ok(n) = s.parse::<usize>() {
                    n.max(1).min(clip.frames.len())
                } else if let Some(n) = clip.labels.get(s) {
                    *n
                } else if self.old_labels {
                    1
                } else {
                    bail!("unknown frame label {s:?}")
                }
            }
        })
    }
    fn context(&self, id: u16) -> ContextData {
        let n = &self.nodes[&id];
        let clip = &self.clips[&id];
        let display = &clip.frames[n.frame - 1];
        let mut foreign = BTreeMap::new();
        let mut parent = n.parent;
        let mut path = "parent".to_string();
        while let Some(id) = parent {
            let p = &self.nodes[&id];
            foreign.insert(
                format!("{path}.currentFrame"),
                Value::Number(p.frame as f64),
            );
            foreign.insert(
                format!("{path}.totalFrames"),
                Value::Number(self.clips[&id].frames.len() as f64),
            );
            parent = p.parent;
            path.push_str(".parent");
        }
        for p in display.values() {
            if let (Some(name), Some(child)) = (p.name(), self.nodes.get(&p.id)) {
                foreign.insert(
                    format!("{name}.currentFrame"),
                    Value::Number(child.frame as f64),
                );
                foreign.insert(
                    format!("{name}.totalFrames"),
                    Value::Number(self.clips[&p.id].frames.len() as f64),
                );
            }
        }
        ContextData {
            names: self.names.get(&id).cloned().unwrap_or_default(),
            children: display.values().filter_map(|p| p.name()).collect(),
            frame: n.frame,
            total_frames: clip.frames.len(),
            label: clip
                .labels
                .iter()
                .filter(|(_, f)| **f <= n.frame)
                .max_by_key(|(_, f)| *f)
                .map(|(l, _)| l.clone()),
            foreign,
            labels: Some(clip.labels.keys().cloned().collect()),
            old_labels: self.old_labels,
        }
    }
    fn attach(&mut self, id: u16, parent: Option<u16>, frame: usize) -> Result<()> {
        if !self.clips.contains_key(&id) {
            return Ok(());
        }
        self.work()?;
        if self.nodes.get(&id).is_some_and(|n| n.active) {
            return Ok(());
        }
        {
            let (record, original_frames) = self
                .nodes
                .remove(&id)
                .map(|n| (n.record, n.original_frames))
                .unwrap_or_default();
            self.nodes.insert(
                id,
                Node {
                    parent,
                    frame,
                    playing: self.clips[&id].frames.len() > 1,
                    state: State::default(),
                    age: 0,
                    record,
                    original_frames,
                    active: true,
                    constructed: false,
                    ran: false,
                },
            );
        }
        self.reconcile(id)
    }
    fn detach(&mut self, id: u16) -> Result<()> {
        let children: Vec<_> = self
            .nodes
            .iter()
            .filter(|(_, n)| n.parent == Some(id) && n.active)
            .map(|(id, _)| *id)
            .collect();
        for child in children {
            self.detach(child)?;
        }
        let class = self.clips[&id].class.clone();
        if Self::runtime(&class) {
            let context = self.context(id);
            let mut e = Evaluator::new(
                class.runtime.as_ref().unwrap(),
                &mut self.nodes.get_mut(&id).unwrap().state,
                &context,
            );
            e.event("Event.REMOVED_FROM_STAGE")?;
            let commands = e.commands;
            self.commands(id, commands)?;
        }
        if let Some(n) = self.nodes.get_mut(&id) {
            n.active = false;
        }
        Ok(())
    }
    fn reconcile(&mut self, id: u16) -> Result<()> {
        ensure!(self.depth < 64, "synchronized hierarchy depth exceeded");
        self.depth += 1;
        let wanted: BTreeSet<_> = self.clips[&id].frames[self.nodes[&id].frame - 1]
            .values()
            .map(|p| p.id)
            .filter(|id| self.clips.contains_key(id))
            .collect();
        let removed: Vec<_> = self
            .nodes
            .iter()
            .filter(|(child, n)| n.active && n.parent == Some(id) && !wanted.contains(child))
            .map(|(id, _)| *id)
            .collect();
        for child in removed {
            self.detach(child)?;
        }
        for child in wanted {
            self.attach(child, Some(id), 1)?;
        }
        self.depth -= 1;
        Ok(())
    }
    fn runtime(class: &Class) -> bool {
        class.runtime.is_some()
            && (class
                .runtime
                .as_ref()
                .is_some_and(|r| r.has_click_listener())
                || class.unsupported.is_some()
                || class.constructor.unsupported.is_some()
                || class.frames.values().any(|p| p.unsupported.is_some()))
    }
    fn initialize(&mut self, id: u16) -> Result<()> {
        if !self.nodes[&id].active || self.nodes[&id].constructed {
            return Ok(());
        }
        self.work()?;
        self.nodes.get_mut(&id).unwrap().constructed = true;
        let children: Vec<_> = self
            .nodes
            .iter()
            .filter(|(_, n)| n.active && n.parent == Some(id))
            .map(|(id, _)| *id)
            .collect();
        for child in children {
            self.initialize(child)?;
        }
        let context = self.context(id);
        let class = self.clips[&id].class.clone();
        let commands = if Self::runtime(&class) {
            let mut e = Evaluator::new(
                class.runtime.as_ref().unwrap(),
                &mut self.nodes.get_mut(&id).unwrap().state,
                &context,
            );
            e.initialize()?;
            e.event("Event.ADDED")?;
            e.event("Event.ADDED_TO_STAGE")?;
            e.commands
        } else {
            class.constructor.commands.clone()
        };
        self.commands(id, commands)?;
        self.configure_children(id);
        let children: Vec<_> = self
            .nodes
            .iter()
            .filter(|(_, n)| n.active && n.parent == Some(id))
            .map(|(id, _)| *id)
            .collect();
        for child in children {
            self.initialize(child)?;
        }
        Ok(())
    }
    fn configure_children(&mut self, id: u16) {
        let values = self.nodes[&id].state.values.clone();
        let display = self.clips[&id].frames[self.nodes[&id].frame - 1].clone();
        for p in display.values() {
            if let (Some(name), Some(node)) = (p.name(), self.nodes.get_mut(&p.id)) {
                let prefix = format!("{name}.");
                for (k, v) in &values {
                    if let Some(k) = k.strip_prefix(&prefix) {
                        node.state.values.insert(k.into(), v.clone());
                    }
                }
            }
        }
    }
    fn run(&mut self, id: u16) -> Result<()> {
        if !self.nodes[&id].active {
            return Ok(());
        }
        self.work()?;
        ensure!(self.depth < 64, "recursive synchronous goto");
        self.depth += 1;
        self.initialize(id)?;
        if !self.nodes[&id].ran {
            self.nodes.get_mut(&id).unwrap().ran = true;
            let context = self.context(id);
            let class = self.clips[&id].class.clone();
            let commands = if Self::runtime(&class) {
                let mut e = Evaluator::new(
                    class.runtime.as_ref().unwrap(),
                    &mut self.nodes.get_mut(&id).unwrap().state,
                    &context,
                );
                e.frame()?;
                e.commands
            } else {
                class
                    .frames
                    .get(&context.frame)
                    .map(|p| p.commands.clone())
                    .unwrap_or_default()
            };
            self.commands(id, commands)?;
            self.configure_children(id);
        }
        for name in self.clips[&id].class.synchronized_children.clone() {
            if !self.clips[&id].frames[self.nodes[&id].frame - 1]
                .values()
                .any(|p| p.name().as_deref() == Some(&name))
            {
                continue;
            }
            let child = self.receiver(id, &name)?;
            let count = self.clips[&child].frames.len();
            let frame = if self.graphic_instance(id) {
                (self.nodes[&id].frame - 1) % count + 1
            } else {
                self.nodes[&id].frame.min(count)
            };
            self.commands(
                id,
                vec![Command {
                    child: Some(name),
                    action: Action::Goto {
                        target: Target::Frame(frame),
                        play: false,
                    },
                }],
            )?;
        }
        let children: Vec<_> = self
            .nodes
            .iter()
            .filter(|(_, n)| n.active && n.parent == Some(id))
            .map(|(id, _)| *id)
            .collect();
        for child in children {
            self.run(child)?;
        }
        self.depth -= 1;
        Ok(())
    }
    fn graphic_instance(&self, id: u16) -> bool {
        let Some(parent) = self.nodes[&id].parent else {
            return false;
        };
        self.clips[&parent].frames[self.nodes[&parent].frame - 1]
            .values()
            .any(|p| {
                p.id == id
                    && p.name().is_some_and(|name| {
                        self.clips[&parent].class.graphic_children.contains(&name)
                    })
            })
    }
    fn receiver(&self, id: u16, path: &str) -> Result<u16> {
        let mut id = id;
        for part in path.split('.') {
            id = if part == "parent" {
                self.nodes[&id]
                    .parent
                    .context("parent control outside requested asset")?
            } else {
                let matches: Vec<_> = self.clips[&id].frames[self.nodes[&id].frame - 1]
                    .values()
                    .filter(|p| p.name().as_deref() == Some(part))
                    .collect();
                ensure!(matches.len() == 1, "missing or ambiguous child {part}");
                matches[0].id
            };
        }
        Ok(id)
    }
    fn commands(&mut self, id: u16, commands: Vec<Command>) -> Result<()> {
        for command in commands {
            self.work()?;
            let target = if let Some(path) = command.child {
                self.receiver(id, &path)?
            } else {
                id
            };
            ensure!(
                self.clips.contains_key(&target),
                "timeline control target is not a MovieClip"
            );
            match command.action {
                Action::Stop => {
                    self.nodes
                        .get_mut(&target)
                        .context("inactive command target")?
                        .playing = false
                }
                Action::Play => {
                    self.nodes
                        .get_mut(&target)
                        .context("inactive command target")?
                        .playing = true
                }
                action => {
                    let (frame, play) = match action {
                        Action::Goto { target: t, play } => (
                            match self.target(target, &t) {
                                Ok(f) => f,
                                Err(e) if e.to_string().starts_with("unknown frame label") => {
                                    let warning = format!("sprite {target}: {e}");
                                    if !self.warnings.contains(&warning) {
                                        self.warnings.push(warning);
                                    }
                                    break;
                                }
                                Err(e) => return Err(e),
                            },
                            play,
                        ),
                        Action::FirstRandomPose => (1, false),
                        _ => unreachable!(),
                    };
                    let n = self
                        .nodes
                        .get_mut(&target)
                        .context("inactive goto target")?;
                    n.playing = play;
                    if n.frame != frame {
                        n.frame = frame;
                        n.ran = false;
                        self.reconcile(target)?;
                        self.run(target)?;
                    }
                }
            }
        }
        Ok(())
    }
    fn click(&mut self, root: u16) -> Result<bool> {
        let mut pending = vec![root];
        while let Some(id) = pending.pop() {
            if let Some(runtime) = self.clips[&id].class.runtime.clone() {
                let context = self.context(id);
                let mut eval = Evaluator::new(
                    &runtime,
                    &mut self.nodes.get_mut(&id).unwrap().state,
                    &context,
                );
                if eval.animation_click()? {
                    let commands = eval.commands;
                    self.commands(id, commands)?;
                    return Ok(true);
                }
            }
            pending.extend(
                self.nodes
                    .iter()
                    .filter(|(_, n)| n.active && n.parent == Some(id))
                    .map(|(id, _)| *id),
            );
        }
        Ok(false)
    }
    fn capture(&mut self) -> Result<()> {
        for (id, n) in &mut self.nodes {
            if !n.active {
                continue;
            }
            let mut frame = self.clips[id].frames[n.frame - 1].clone();
            for warning in &n.state.faults {
                if !self.warnings.contains(warning) {
                    self.warnings.push(warning.clone());
                }
            }
            for (key, value) in &n.state.values {
                if let Some(layer) = key
                    .strip_prefix("@layer:")
                    .and_then(|s| s.strip_suffix(".visible"))
                {
                    let Value::Bool(visible) = value else {
                        bail!("nonboolean host visibility");
                    };
                    if let Some(previous) = self.host_visibility.insert(layer.into(), *visible) {
                        ensure!(previous==*visible,"animated/conflicting host-layer visibility requires composition scheduling");
                    }
                }
            }
            for p in frame.values_mut() {
                if n.state.values.get("visible") == Some(&Value::Bool(false))
                    || p.name().is_some_and(|name| {
                        n.state.values.get(&format!("{name}.visible")) == Some(&Value::Bool(false))
                    })
                {
                    p.set_visible(false);
                }
            }
            if let Some(previous) = n.record.get(n.age) {
                ensure!(
                    previous == &frame,
                    "recreated clip depends on external clock; needs another instance definition"
                );
            } else {
                n.record.push(frame);
                n.original_frames.push(n.frame);
            }
            n.age += 1;
        }
        Ok(())
    }
    fn tick(&mut self) -> Result<()> {
        let active: Vec<_> = self
            .nodes
            .iter()
            .filter(|(_, n)| n.active)
            .map(|(id, _)| *id)
            .collect();
        for id in &active {
            if !self.nodes[id].active {
                continue;
            }
            let class = self.clips[id].class.clone();
            if Self::runtime(&class) {
                let context = self.context(*id);
                let mut e = Evaluator::new(
                    class.runtime.as_ref().unwrap(),
                    &mut self.nodes.get_mut(id).unwrap().state,
                    &context,
                );
                e.event("Event.ENTER_FRAME")?;
                let commands = e.commands;
                self.commands(*id, commands)?;
            }
        }
        for id in active {
            if !self.nodes[&id].active {
                continue;
            }
            self.work()?;
            let n = self.nodes.get_mut(&id).unwrap();
            if n.playing && self.clips[&id].frames.len() > 1 {
                n.frame = n.frame % self.clips[&id].frames.len() + 1;
                n.ran = false;
                self.reconcile(id)?;
            }
        }
        let held: Vec<_> = self
            .nodes
            .iter()
            .filter(|(_, n)| n.active && n.ran)
            .map(|(id, _)| *id)
            .collect();
        for id in held {
            let class = self.clips[&id].class.clone();
            if Self::runtime(&class) {
                let context = self.context(id);
                let mut e = Evaluator::new(
                    class.runtime.as_ref().unwrap(),
                    &mut self.nodes.get_mut(&id).unwrap().state,
                    &context,
                );
                e.held_events()?;
                let commands = e.commands;
                self.commands(id, commands)?;
                self.configure_children(id);
            }
        }
        for id in self.roots.clone() {
            self.run(id)?;
        }
        Ok(())
    }
    fn signature(&self) -> Result<String> {
        let values: Vec<_> = self
            .nodes
            .iter()
            .filter(|(_, n)| n.active)
            .map(|(id, n)| (*id, n.parent, n.frame, n.playing, &n.state))
            .collect();
        crate::digest(&values)
    }
}
pub(crate) fn normalize(
    source: &[u8],
    swf: &Swf,
    scripts: &BTreeMap<String, Class>,
    requests: &[SymbolRequest],
) -> Result<Normalized> {
    let isolated = instances::isolate(source, swf, scripts, requests)?;
    let mut clips = BTreeMap::new();
    for id in isolated.names.keys() {
        let payload = &isolated.swf.sprites[id];
        let frames = display::snapshots(payload)?;
        ensure!(!frames.is_empty(), "empty synchronized clip");
        let mut labels = BTreeMap::new();
        let mut frame = 1;
        for (code, data) in swf::tags(payload, 4)? {
            if code == 1 {
                frame += 1;
            }
            if code == 43 {
                labels.insert(swf::cstring(data, &mut 0)?, frame);
            }
        }
        let class = isolated
            .swf
            .symbols
            .iter()
            .find(|(n, _)| n == id)
            .and_then(|(_, name)| isolated.scripts.get(&name.to_lowercase()))
            .cloned()
            .unwrap_or_default();

        clips.insert(
            *id,
            Clip {
                frames,
                labels,
                class,
            },
        );
    }
    let mut scene = Scene {
        swf: &isolated.swf,
        clips,
        nodes: BTreeMap::new(),
        names: &isolated.names,
        roots: isolated.requests.iter().map(|r| r.character_id).collect(),
        budget: 2_000_000,
        depth: 0,
        old_labels: source[3] < 11,
        host_visibility: BTreeMap::new(),
        warnings: Vec::new(),
    };
    for r in &isolated.requests {
        scene.attach(r.character_id, None, if r.click { 1 } else { r.frame })?;
        scene.nodes.get_mut(&r.character_id).unwrap().playing =
            r.root_timeline_frames > 1 || scene.clips[&r.character_id].class.frames.len() > 0;
    }
    for id in scene.roots.clone() {
        scene.run(id)?;
    }
    for r in &isolated.requests {
        if r.click {
            // Frame-one setup runs even when the render host selects a later idle label.
            if r.frame > 1 && scene.nodes[&r.character_id].frame != r.frame {
                let play = scene.nodes[&r.character_id].playing;
                scene.commands(
                    r.character_id,
                    vec![Command {
                        child: None,
                        action: Action::Goto {
                            target: Target::Frame(r.frame),
                            play,
                        },
                    }],
                )?;
            }
            scene.click(r.character_id)?;
        }
    }
    let capture_end = requests
        .iter()
        .map(|r| r.capture_end)
        .max()
        .unwrap_or(1)
        .clamp(1, 12008);
    let mut seen = BTreeMap::new();
    let mut loop_start = None;
    let mut ticks: usize = 0;
    while ticks < capture_end {
        let signature = scene.signature()?;
        if let Some(start) = seen.get(&signature) {
            loop_start = Some(*start);
            break;
        }
        seen.insert(signature, ticks);
        scene.capture()?;
        ticks += 1;
        scene.tick()?;
    }
    let mut replacements = BTreeMap::new();
    let mut decisions = Vec::new();
    for (id, node) in &scene.nodes {
        if node.record.is_empty() {
            continue;
        }
        let mut frames = node.record.clone();
        if node.active {
            if let Some(start) = loop_start {
                // A continuously alive clip uses the global loop with its own age offset.
                let born = ticks.saturating_sub(node.age);
                let start = start.saturating_sub(born);
                if start < frames.len() && start > 0 {
                    let cycle = frames[start..].to_vec();
                    while frames.len() < capture_end {
                        frames.push(cycle[(frames.len() - start) % cycle.len()].clone());
                    }
                }
            }
        }
        let constant = frames.windows(2).all(|w| w[0] == w[1]);
        if constant {
            frames.truncate(1);
        }
        replacements.insert(*id, display::write_frames(*id, &frames)?);
        // Omit unchanged one-frame definitions from potentially large job logs.
        if scene.clips[id].frames.len() == 1
            && node
                .record
                .iter()
                .all(|frame| frame == &scene.clips[id].frames[0])
        {
            continue;
        }
        decisions.push(Decision {
            character_id: *id,
            class_name: scene
                .swf
                .symbols
                .iter()
                .find(|(n, _)| n == id)
                .map(|(_, n)| n.clone())
                .unwrap_or_default(),
            selection: if constant
                && node
                    .original_frames
                    .iter()
                    .all(|f| *f == node.original_frames[0])
            {
                Selection::Hold {
                    frame: node.original_frames[0],
                }
            } else {
                Selection::Sequence {
                    frames: node.original_frames.clone(),
                    loop_start,
                }
            },
        });
    }
    let mut requests = isolated.requests.clone();
    for r in &mut requests {
        r.frame = 1;
        r.root_timeline_frames = swf::u16_at(&replacements[&r.character_id], 2)? as usize;
    }
    Ok(Normalized {
        bytes: swf::replace_sprites(&isolated.bytes, &replacements)?,
        requests,
        decisions,
        symbol_aliases: isolated.aliases,
        host_visibility: scene.host_visibility,
        warnings: scene.warnings,
    })
}
