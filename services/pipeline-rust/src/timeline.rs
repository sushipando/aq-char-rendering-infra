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
    Hold { frame: usize },
    Loop { first: usize, last: usize },
}
impl Selection {
    fn range(&self) -> (usize, usize) {
        match *self {
            Self::Hold { frame } => (frame, frame),
            Self::Loop { first, last } => (first, last),
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
    fn read(payload: &[u8], budget: &mut usize) -> Result<Self> {
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
                // This resolver handles AS3 callbacks, not legacy AVM1 bytecode.
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
    fn target(&self, target: &Target) -> Result<usize> {
        let frame = match target {
            Target::Frame(frame) => *frame,
            Target::Label(label) => *self
                .labels
                .get(label)
                .with_context(|| format!("unknown frame label {label:?}"))?,
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
    selections: BTreeMap<u16, Selection>,
    active: BTreeSet<u16>,
    // Same symbol used with incompatible per-instance state must not be globally rewritten.
    entries: BTreeMap<u16, Entry>,
    budget: usize,
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
                let clip = Clip::read(&self.swf.sprites[&id], &mut self.budget)?;
                ensure!(
                    previous.play == entry.play
                        && clip.target(&previous.target)? == clip.target(&entry.target)?,
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
        let clip = Clip::read(&self.swf.sprites[&id], &mut self.budget)?;
        let name = self
            .swf
            .symbols
            .iter()
            .find(|(n, _)| *n == id)
            .map(|(_, s)| s.to_lowercase())
            .unwrap_or_default();
        let class = self.scripts.get(&name).cloned().unwrap_or_default();
        ensure!(
            class.unsupported.is_none(),
            "unsupported script registration: {:?}",
            class.unsupported
        );
        ensure!(
            class.constructor.unsupported.is_none(),
            "unsupported constructor: {:?}",
            class.constructor.unsupported
        );
        let mut frame = clip.target(&entry.target)?;
        let mut named_state = matches!(entry.target, Target::Label(_))
            || clip.labels.iter().any(|(label, at)| {
                *at == frame
                    && matches!(
                        label.to_ascii_lowercase().as_str(),
                        "idle" | "idel" | "id" | "ready"
                    )
            });
        let mut playing = entry.play;
        let mut trace = Vec::new();
        let mut seen = BTreeMap::new();
        let mut child_entries: BTreeMap<(u16, usize), Entry> = BTreeMap::new();
        let selection;
        loop {
            self.budget = self
                .budget
                .checked_sub(1)
                .context("timeline execution work limit exceeded")?;
            if let Some(&start) = seen.get(&frame) {
                ensure!(playing, "nonterminating stopped-frame script cycle");
                let cycle: &[usize] = &trace[start..];
                ensure!(
                    cycle.windows(2).all(|w| w[1] == w[0] + 1),
                    "non-contiguous scripted timeline loop"
                );
                selection = Selection::Loop {
                    first: cycle[0],
                    last: *cycle.last().unwrap(),
                };
                break;
            }
            seen.insert(frame, trace.len());
            trace.push(frame);
            let mut destination = frame;
            let mut commands = Vec::<Command>::new();
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
                        _ => bail!("frame {frame}: child stop/play without an explicit target is unsupported"),
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
                            named_state |= matches!(target, Target::Label(_));
                            destination = clip.target(&target)?;
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
            // Never silently run from the selected idle segment into another named state.
            let state = |label: &str| {
                matches!(
                    label.to_ascii_lowercase().as_str(),
                    "idle"
                        | "idel"
                        | "id"
                        | "ready"
                        | "walk"
                        | "run"
                        | "attack"
                        | "death"
                        | "dead"
                        | "hurt"
                )
            };
            if destination == frame && next != trace[0] {
                ensure!(!clip.labels.iter().any(|(label, at)| *at == next && (named_state || state(label))), "frame {frame}: implicit transition into another animation state; no proven idle stop/loop");
            }
            frame = next;
        }
        let (first, last) = selection.range();
        let mut children = BTreeSet::new();
        for f in &clip.frames[first - 1..last] {
            children.extend(f.display.values().cloned());
        }
        // A child command during a repeating parent would reset its playhead each cycle.
        // Our independent-child export cannot represent that synchronization safely.
        if matches!(selection, Selection::Loop { .. }) {
            ensure!(
                child_entries.is_empty(),
                "repeating parent controls child playheads; synchronized timelines are unsupported"
            );
        }
        for child in children {
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
}

pub fn normalize(
    source: &[u8],
    swf: &Swf,
    scripts: &BTreeMap<String, Class>,
    requests: &[SymbolRequest],
) -> Result<Normalized> {
    let mut resolver = Resolver {
        swf,
        scripts,
        selections: BTreeMap::new(),
        active: BTreeSet::new(),
        entries: BTreeMap::new(),
        budget: 2_000_000,
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
            let (first, last) = selection.range();
            request.frame = 1;
            request.root_timeline_frames = last - first + 1;
        }
    }
    Ok(Normalized {
        bytes: swf::replace_sprites(source, &replacements)?,
        requests: effective,
        decisions,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
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
        }
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
        let child = Clip::read(&updated.sprites[&2], &mut 1000).unwrap();
        assert_eq!(child.frames.len(), 1);
        assert_eq!(child.frames[0].display[&1].id, 3);
        let root = Clip::read(&updated.sprites[&1], &mut 1000).unwrap();
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
        assert!(format!("{error:#}").contains("reachable frame 1"));
    }

    #[test]
    fn no_guessed_state_bleed_missing_labels_or_conflicting_instances() {
        let (bytes, swf) = fixture(vec![
            (
                1,
                "Root",
                vec![[place(2, 1, "a"), place(2, 2, "b")].concat()],
            ),
            (2, "Child", vec![tag(43, b"Idle\0"), tag(43, b"Walk\0")]),
        ]);
        for body in [
            "this.a.gotoAndStop(1);this.b.gotoAndStop(2);stop();",
            "this.a.gotoAndStop(\"Missing\");stop();",
            "this.a.gotoAndPlay(\"Idle\");stop();",
        ] {
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
            Clip::read(&updated.sprites[&1], &mut 1000).unwrap().frames[0]
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
        assert!(format!(
            "{:#}",
            normalize(&bytes, &swf, &metadata, &[request()])
                .err()
                .unwrap()
        )
        .contains("implicit transition"));
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
