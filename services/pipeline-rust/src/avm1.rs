//! Translate bounded, literal AVM1 playback bytecode into shared timeline programs.
//! This reads DoAction records, not FFDec source text. No asset-specific approvals
//! are needed for playback. Dynamic code and target changes remain explicit errors.
use crate::{
    model::SymbolRequest,
    script::Class,
    swf::{self, Swf},
};
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub(crate) struct Prepared {
    pub bytes: Vec<u8>,
    pub swf: Swf,
    pub scripts: BTreeMap<String, Class>,
    pub requests: Vec<SymbolRequest>,
    pub warnings: Vec<String>,
}

struct Reader<'a>(&'a [u8]);
impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let bytes = self.0.get(..n).context("truncated AVM1 action")?;
        self.0 = &self.0[n..];
        Ok(bytes)
    }
    fn byte(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn short(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into()?))
    }
    fn string(&mut self) -> Result<String> {
        let end = self
            .0
            .iter()
            .position(|b| *b == 0)
            .context("unterminated AVM1 string")?;
        // Avoid silently changing a label in old code-page encoded SWFs.
        let value = std::str::from_utf8(self.take(end)?)
            .context("non-UTF8 AVM1 label")?
            .to_owned();
        self.take(1)?;
        Ok(value)
    }
}

#[derive(Clone, Debug)]
enum Literal {
    Number(f64),
    String(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Control {
    Stop,
    Play,
    Next,
    Previous,
    Goto { frame: usize, play: bool },
}

struct Compiler<'a> {
    labels: &'a BTreeMap<String, usize>,
    total: usize,
    commands: Vec<Control>,
    warnings: Vec<String>,
}
impl Compiler<'_> {
    fn goto(&mut self, frame: usize, play: bool) {
        self.commands.push(Control::Goto {
            frame: frame.min(self.total),
            play,
        });
    }
    fn label(&mut self, label: &str) -> Option<usize> {
        let frame = self.labels.get(&label.to_ascii_lowercase()).copied();
        if frame.is_none() {
            self.warnings
                .push(format!("AVM1: unknown frame label {label:?}; goto ignored"));
        }
        frame
    }
    fn stack_target(&mut self, value: Literal, bias: u16, play: bool) -> Result<()> {
        let number = match value {
            Literal::Number(n) => {
                ensure!(
                    n.is_finite() && n.fract() == 0.0 && n.abs() <= i32::MAX as f64,
                    "unsupported AVM1 non-integral/out-of-range frame number"
                );
                n as i64
            }
            Literal::String(s) => {
                ensure!(
                    !s.contains([':', '/', '.']),
                    "AVM1 target paths require instance lookup"
                );
                if let Ok(n) = s.parse::<i32>() {
                    n as i64
                } else if let Some(n) = self.label(&s) {
                    n as i64
                } else {
                    return Ok(());
                }
            }
        };
        let frame = number + i64::from(bias);
        // AVM1 nonpositive gotos have no effect, including on the play state.
        if frame > 0 {
            ensure!(
                frame <= u16::MAX as i64,
                "unsupported AVM1 overflowing frame number"
            );
            self.goto(frame as usize, play);
        }
        Ok(())
    }
    fn block(&mut self, bytes: &[u8], budget: &mut usize) -> Result<()> {
        ensure!(
            bytes.len() <= 1024 * 1024,
            "AVM1 action block exceeds 1 MiB"
        );
        let mut reader = Reader(bytes);
        let mut pool = Vec::<String>::new();
        let mut stack = Vec::<Literal>::new();
        loop {
            *budget = budget
                .checked_sub(1)
                .context("AVM1 instruction work limit exceeded")?;
            let offset = bytes.len() - reader.0.len();
            let opcode = reader.byte()?;
            if opcode == 0 {
                ensure!(reader.0.is_empty(), "trailing bytes after AVM1 End");
                ensure!(
                    stack.is_empty(),
                    "AVM1 block leaves an unresolved operand stack"
                );
                return Ok(());
            }
            let len = if opcode >= 0x80 {
                reader.short()? as usize
            } else {
                0
            };
            let mut operands = Reader(reader.take(len)?);
            match opcode {
                0x04 => self.commands.push(Control::Next),
                0x05 => self.commands.push(Control::Previous),
                0x06 => self.commands.push(Control::Play),
                0x07 => self.commands.push(Control::Stop),
                0x17 => {
                    stack.pop().context("AVM1 Pop stack underflow")?;
                }
                0x4c => stack.push(
                    stack
                        .last()
                        .context("AVM1 duplicate stack underflow")?
                        .clone(),
                ),
                0x4d => {
                    ensure!(stack.len() >= 2, "AVM1 swap stack underflow");
                    let n = stack.len();
                    stack.swap(n - 1, n - 2);
                }
                0x81 => self.goto(usize::from(operands.short()?) + 1, false),
                0x88 => {
                    let count = operands.short()? as usize;
                    ensure!(count <= 4096, "AVM1 constant pool limit exceeded");
                    pool = (0..count)
                        .map(|_| operands.string())
                        .collect::<Result<_>>()?;
                }
                0x8c => {
                    let label = operands.string()?;
                    if let Some(frame) = self.label(&label) {
                        self.goto(frame, false);
                    }
                }
                0x96 => {
                    while !operands.0.is_empty() {
                        let literal = match operands.byte()? {
                            0 => Literal::String(operands.string()?),
                            1 => Literal::Number(
                                f32::from_le_bytes(operands.take(4)?.try_into()?) as f64
                            ),
                            6 => {
                                // SWF doubles store the two little-endian 32-bit words high first.
                                let bytes = operands.take(8)?;
                                let mut le = [0; 8];
                                le[..4].copy_from_slice(&bytes[4..]);
                                le[4..].copy_from_slice(&bytes[..4]);
                                Literal::Number(f64::from_le_bytes(le))
                            }
                            7 => Literal::Number(
                                i32::from_le_bytes(operands.take(4)?.try_into()?) as f64
                            ),
                            kind @ (8 | 9) => {
                                let index = if kind == 8 {
                                    operands.byte()? as usize
                                } else {
                                    operands.short()? as usize
                                };
                                Literal::String(
                                    pool.get(index)
                                        .context("invalid AVM1 constant pool index")?
                                        .clone(),
                                )
                            }
                            kind => {
                                bail!("unsupported AVM1 Push value type {kind} at byte {offset}")
                            }
                        };
                        stack.push(literal);
                        ensure!(stack.len() <= 256, "AVM1 operand stack limit exceeded");
                    }
                }
                0x9f => {
                    let flags = operands.byte()?;
                    ensure!(flags & !3 == 0, "invalid AVM1 GotoFrame2 flags");
                    let bias = if flags & 2 != 0 { operands.short()? } else { 0 };
                    self.stack_target(
                        stack.pop().context("AVM1 GotoFrame2 stack underflow")?,
                        bias,
                        flags & 1 != 0,
                    )?;
                }
                _ => bail!("unsupported AVM1 opcode 0x{opcode:02x} at byte {offset}"),
            }
            ensure!(
                operands.0.is_empty(),
                "invalid AVM1 opcode 0x{opcode:02x} operand length at byte {offset}"
            );
            ensure!(stack.len() <= 256, "AVM1 operand stack limit exceeded");
        }
    }
}

/// Translate the requested display trees only. Approved background setup remains
/// byte-hash-bound; all other DoActions must be fully decoded. Runtime action tags
/// are removed only after their commands have been captured for timeline baking.
pub(crate) fn prepare(
    source: &[u8],
    swf: &Swf,
    scripts: &BTreeMap<String, Class>,
    requests: &[SymbolRequest],
    inert: &BTreeSet<String>,
) -> Result<Option<Prepared>> {
    // Most SWFs have no AVM1 actions. Avoid copying their source/metadata.
    if !swf
        .sprites
        .values()
        .any(|p| swf::tags(p, 4).is_ok_and(|tags| tags.iter().any(|(code, _)| *code == 12)))
    {
        return Ok(None);
    }
    let body = swf::decompress(source)?;
    let offset = (5 + 4 * (body[0] as usize >> 3)).div_ceil(8) + 4;
    if swf::tags(&body, offset)?
        .iter()
        .any(|(c, d)| *c == 69 && d.first().is_some_and(|f| f & 8 != 0))
    {
        return Ok(None); // Never reinterpret AVM1 bytes as AS3 callbacks.
    }
    let mut pending: Vec<_> = requests.iter().map(|r| r.character_id).collect();
    let mut visited = BTreeSet::new();
    let mut replacements = BTreeMap::new();
    let mut programs = BTreeMap::new();
    let mut warnings = Vec::new();
    let mut budget = 2_000_000usize;
    let mut translated = false;
    while let Some(id) = pending.pop() {
        if !visited.insert(id) {
            continue;
        }
        let Some(payload) = swf.sprites.get(&id) else {
            continue;
        };
        let tags = swf::tags(payload, 4)?;
        let mut labels = BTreeMap::new();
        let mut frame = 1;
        for (code, data) in &tags {
            budget = budget
                .checked_sub(1)
                .context("AVM1 tree work limit exceeded")?;
            if let Some((_, Some(child), _, _)) = swf::instance(*code, data)? {
                pending.push(child);
            }
            if *code == 43 {
                let label = swf::cstring(data, &mut 0)?.to_ascii_lowercase();
                // Flash retains the first frame with a given AVM1 label.
                labels.entry(label).or_insert(frame);
            }
            if *code == 1 {
                frame += 1;
            }
        }
        let total = swf::u16_at(payload, 2)? as usize;
        ensure!(
            frame == total + 1 && labels.values().all(|n| *n <= total),
            "invalid AVM1 timeline frames/labels: sprite {id}"
        );
        let mut rewritten = payload[..4].to_vec();
        let mut class = Class::default();
        let mut compiler = Compiler {
            labels: &labels,
            total,
            commands: Vec::new(),
            warnings: Vec::new(),
        };
        let mut frame = 1;
        let mut removed = false;
        let mut frame_script = false;
        for (code, data) in tags {
            if code == 12 {
                ensure!(
                    frame <= total && total > 0,
                    "AVM1 action outside timeline: sprite {id}, frame {frame}"
                );
                removed = true;
                if inert.contains(&crate::sha256(data)) {
                    continue;
                }
                compiler
                    .block(data, &mut budget)
                    .with_context(|| format!("AVM1 timeline: sprite {id}, frame {frame}"))?;
                translated = true;
                frame_script = true;
                continue;
            }
            ensure!(
                code != 59,
                "AVM1 DoInitAction requires runtime initialization support: sprite {id}"
            );
            if code == 1 {
                if frame_script {
                    class
                        .avm1_frames
                        .insert(frame, std::mem::take(&mut compiler.commands));
                }
                frame_script = false;
                frame += 1;
            }
            swf::write_tag(&mut rewritten, code, data);
        }
        warnings.extend(
            compiler
                .warnings
                .into_iter()
                .map(|s| format!("sprite {id}: {s}")),
        );
        if removed {
            replacements.insert(id, rewritten);
        }
        if !class.avm1_frames.is_empty() {
            programs.insert(id, class);
        }
    }
    if !translated {
        return Ok(None);
    }
    // DoInitAction normally lives in the movie's tag stream, outside DefineSprite.
    // It can initialize AS2 classes before their instances are placed.
    for (code, data) in swf::tags(&body, offset)? {
        if code == 59 {
            bail!(
                "AVM1 DoInitAction requires runtime initialization support: sprite {}",
                swf::u16_at(data, 0)?
            );
        }
    }
    let mut scripts = scripts.clone();
    let mut symbols = swf.symbols.clone();
    let mut requests = requests.to_vec();
    for (id, class) in programs {
        let name = if let Some((_, name)) = symbols.iter().find(|(n, _)| *n == id) {
            name.clone()
        } else {
            let mut name = format!("AqwAvm1Timeline{id}");
            while symbols.iter().any(|(_, n)| n.eq_ignore_ascii_case(&name))
                || scripts.contains_key(&name.to_lowercase())
            {
                name.push('_');
            }
            symbols.push((id, name.clone()));
            name
        };
        ensure!(
            !scripts.contains_key(&name.to_lowercase()),
            "mixed AVM1/AS3 script metadata for sprite {id}"
        );
        scripts.insert(name.to_lowercase(), class);
        for request in requests.iter_mut().filter(|r| r.character_id == id) {
            request.class_name = name.clone();
        }
    }
    let bytes = swf::replace_dictionary(source, &replacements, &symbols)?;
    let swf = Swf::parse(&bytes)?;
    Ok(Some(Prepared {
        bytes,
        swf,
        scripts,
        requests,
        warnings,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::timeline::{self, Selection};

    fn action(opcode: u8, operands: &[u8]) -> Vec<u8> {
        let mut bytes = vec![opcode];
        if opcode >= 0x80 {
            bytes.extend((operands.len() as u16).to_le_bytes());
        }
        bytes.extend(operands);
        bytes
    }
    fn decode(bytes: &[u8]) -> Result<Vec<Control>> {
        let labels = BTreeMap::from([("move".into(), 3), ("2".into(), 4)]);
        let mut compiler = Compiler {
            labels: &labels,
            total: 5,
            commands: vec![],
            warnings: vec![],
        };
        compiler.block(bytes, &mut 1000)?;
        Ok(compiler.commands)
    }
    fn tag(code: u16, data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        swf::write_tag(&mut out, code, data);
        out
    }
    fn actions(bytes: &[u8]) -> Vec<u8> {
        tag(12, bytes)
    }
    fn place(id: u16, depth: u16) -> Vec<u8> {
        let mut bytes = vec![6];
        bytes.extend(depth.to_le_bytes());
        bytes.extend(id.to_le_bytes());
        bytes.push(0);
        tag(26, &bytes)
    }
    fn goto(label: &str) -> Vec<u8> {
        let mut s = label.as_bytes().to_vec();
        s.push(0);
        let mut b = action(0x8c, &s);
        b.extend([6, 0]);
        actions(&b)
    }
    fn movie(parts: Vec<(u16, Vec<Vec<u8>>)>) -> (Vec<u8>, Swf) {
        let mut body = vec![0, 0, 24, 1, 0];
        for (id, frames) in parts {
            let mut sprite = id.to_le_bytes().to_vec();
            sprite.extend((frames.len() as u16).to_le_bytes());
            for frame in frames {
                sprite.extend(frame);
                sprite.extend(tag(1, &[]));
            }
            sprite.extend(tag(0, &[]));
            body.extend(tag(39, &sprite));
        }
        body.extend(tag(1, &[]));
        body.extend(tag(0, &[]));
        let mut bytes = b"FWS\x08".to_vec();
        bytes.extend(((body.len() + 8) as u32).to_le_bytes());
        bytes.extend(body);
        let swf = Swf::parse(&bytes).unwrap();
        (bytes, swf)
    }
    fn request() -> SymbolRequest {
        SymbolRequest {
            key: "pet".into(),
            class_name: "Root".into(),
            character_id: 1,
            frame: 1,
            root_timeline_frames: 1,
            click: false,
            ancestor_names: vec![],
            capture_end: 12,
        }
    }
    fn normalized(parts: Vec<(u16, Vec<Vec<u8>>)>) -> timeline::Normalized {
        let (bytes, swf) = movie(parts);
        timeline::normalize(&bytes, &swf, &BTreeMap::new(), &[request()]).unwrap()
    }
    #[test]
    fn reads_direct_controls_without_source_text() {
        let mut bytes = vec![7, 6, 4, 5];
        bytes.extend(action(0x81, &[1, 0]));
        bytes.extend(action(0x8c, b"MoVe\0"));
        bytes.extend([6, 0]);
        assert_eq!(
            decode(&bytes).unwrap(),
            vec![
                Control::Stop,
                Control::Play,
                Control::Next,
                Control::Previous,
                Control::Goto {
                    frame: 2,
                    play: false
                },
                Control::Goto {
                    frame: 3,
                    play: false
                },
                Control::Play
            ]
        );
        // GotoLabel "2" is a label, whereas GotoFrame2's string "2" is numeric.
        let mut bytes = action(0x8c, b"2\0");
        bytes.push(0);
        assert_eq!(
            decode(&bytes).unwrap(),
            vec![Control::Goto {
                frame: 4,
                play: false
            }]
        );
    }
    #[test]
    fn literal_stack_pool_and_scene_bias() {
        let mut bytes = action(0x88, b"\x01\x00MOVE\0");
        bytes.extend(action(0x96, &[8, 0]));
        bytes.extend(action(0x9f, &[1]));
        bytes.extend(action(0x96, b"\x002\0"));
        bytes.extend(action(0x9f, &[2, 1, 0]));
        bytes.push(0);
        assert_eq!(
            decode(&bytes).unwrap(),
            vec![
                Control::Goto {
                    frame: 3,
                    play: true
                },
                Control::Goto {
                    frame: 3,
                    play: false
                }
            ]
        );
        for n in [-2i32, 0, 2, 100] {
            let mut operand = vec![7];
            operand.extend(n.to_le_bytes());
            let mut bytes = action(0x96, &operand);
            bytes.extend(action(0x9f, &[0]));
            bytes.push(0);
            let expected = if n <= 0 {
                vec![]
            } else {
                vec![Control::Goto {
                    frame: (n as usize).min(5),
                    play: false,
                }]
            };
            assert_eq!(decode(&bytes).unwrap(), expected);
        }
    }
    #[test]
    fn missing_labels_do_not_reset_playback() {
        let mut bytes = vec![6];
        bytes.extend(action(0x8c, b"missing\0"));
        bytes.push(0);
        assert_eq!(decode(&bytes).unwrap(), vec![Control::Play]);
    }
    #[test]
    fn malformed_dynamic_and_external_actions_are_not_ignored() {
        for bytes in [
            vec![7],
            vec![7, 0, 6],
            vec![0x81, 2, 0, 1],
            vec![0x81, 3, 0, 0, 0, 0, 0],
            vec![0x17, 0],
            vec![0x9f, 1, 0, 0, 0],
            vec![7, 0x1d, 0],
            vec![0x83, 2, 0, 0, 0, 0],
            vec![0x8b, 1, 0, 0, 0],
            vec![0x99, 2, 0, 0, 0, 0],
        ] {
            assert!(decode(&bytes).is_err(), "{bytes:?}");
        }
        let mut bytes = action(0x96, b"\x00_parent:Move\0");
        bytes.extend(action(0x9f, &[1]));
        bytes.push(0);
        assert!(decode(&bytes)
            .unwrap_err()
            .to_string()
            .contains("target paths"));
    }
    #[test]
    fn stopped_parent_retains_child_animation() {
        let result = normalized(vec![
            (1, vec![[place(2, 1), actions(&[7, 0])].concat(), vec![]]),
            (2, vec![place(101, 1), place(102, 1), place(103, 1)]),
        ]);
        assert!(result
            .decisions
            .iter()
            .any(|d| d.character_id == 1 && d.selection == Selection::Hold { frame: 1 }));
        let parsed = Swf::parse(&result.bytes).unwrap();
        let frames = crate::display::snapshots(&parsed.sprites[&2]).unwrap();
        assert_eq!(
            frames.iter().map(|f| f[&1].id).collect::<Vec<_>>(),
            vec![101, 102, 103]
        );
    }
    #[test]
    fn finite_playback_keeps_intro_and_final_pose() {
        let result = normalized(vec![(
            1,
            vec![
                place(101, 1),
                place(102, 1),
                [place(103, 1), actions(&[7, 0])].concat(),
            ],
        )]);
        assert_eq!(
            result.decisions[0].selection,
            Selection::Sequence {
                frames: vec![1, 2, 3],
                loop_start: Some(2)
            }
        );
        let parsed = Swf::parse(&result.bytes).unwrap();
        let frames = crate::display::snapshots(&parsed.sprites[&1]).unwrap();
        assert_eq!(
            frames.iter().take(4).map(|f| f[&1].id).collect::<Vec<_>>(),
            vec![101, 102, 103, 103]
        );
    }
    #[test]
    fn label_loops_bake_without_live_action_tags() {
        let result = normalized(vec![(
            1,
            vec![
                [tag(43, b"Move\0"), place(101, 1)].concat(),
                place(102, 1),
                goto("mOvE"),
            ],
        )]);
        assert_eq!(
            result.decisions[0].selection,
            Selection::Sequence {
                frames: vec![1, 2],
                loop_start: Some(0)
            }
        );
        let parsed = Swf::parse(&result.bytes).unwrap();
        assert!(swf::tags(&parsed.sprites[&1], 4)
            .unwrap()
            .iter()
            .all(|(code, _)| *code != 12));
    }
    #[test]
    fn goto_destination_callback_runs_after_current_block() {
        let mut jump = action(0x81, &[1, 0]);
        jump.extend([6, 0]);
        let result = normalized(vec![(
            1,
            vec![
                actions(&jump),
                [place(102, 1), actions(&[7, 0])].concat(),
                place(103, 1),
            ],
        )]);
        assert_eq!(result.decisions[0].selection, Selection::Hold { frame: 2 });
    }
    #[test]
    fn queued_intermediate_callbacks_and_relative_controls_use_live_frame() {
        let mut jump = action(0x81, &[1, 0]);
        jump.extend([4, 6, 0]);
        let result = normalized(vec![(
            1,
            vec![
                actions(&jump),
                actions(&[7, 0]),
                place(103, 1),
                place(104, 1),
            ],
        )]);
        // goto(2), nextFrame() -> 3, play(); the queued frame-2 stop runs last.
        assert_eq!(result.decisions[0].selection, Selection::Hold { frame: 3 });
    }
    #[test]
    fn multiple_action_tags_keep_order_and_instances_keep_independent_state() {
        let (bytes, swf) = movie(vec![(
            1,
            vec![
                [place(101, 1), actions(&[7, 0]), actions(&[6, 0])].concat(),
                [place(102, 1), actions(&[7, 0])].concat(),
            ],
        )]);
        let mut second = request();
        second.key = "other".into();
        second.frame = 2;
        let result =
            timeline::normalize(&bytes, &swf, &BTreeMap::new(), &[request(), second]).unwrap();
        assert_ne!(
            result.requests[0].character_id,
            result.requests[1].character_id
        );
        assert!(result.decisions.iter().any(
            |d| matches!(&d.selection, Selection::Sequence {frames,..} if frames == &vec![1,2])
        ));
        assert!(result
            .decisions
            .iter()
            .any(|d| d.selection == Selection::Hold { frame: 2 }));
    }
    #[test]
    fn unsupported_actions_have_sprite_frame_and_opcode_context() {
        let (bytes, swf) = movie(vec![(1, vec![actions(&[7, 0x1d, 0])])]);
        let error = timeline::normalize(&bytes, &swf, &BTreeMap::new(), &[request()])
            .err()
            .unwrap();
        let text = format!("{error:#}");
        assert!(
            text.contains("sprite 1, frame 1") && text.contains("opcode 0x1d"),
            "{text}"
        );
    }
    #[test]
    fn background_setup_approval_stays_separate_from_playback_support() {
        let setup = b"opaque approved setup";
        let (bytes, swf) = movie(vec![
            (1, vec![[place(2, 1), actions(setup)].concat()]),
            (2, vec![actions(&[7, 0]), vec![]]),
        ]);
        assert!(timeline::normalize(&bytes, &swf, &BTreeMap::new(), &[request()]).is_err());
        let result = timeline::normalize_with_avm1(
            &bytes,
            &swf,
            &BTreeMap::new(),
            &[request()],
            &BTreeSet::from([crate::sha256(setup)]),
        )
        .unwrap();
        assert!(result
            .decisions
            .iter()
            .any(|d| d.character_id == 2 && d.selection == Selection::Hold { frame: 1 }));
    }
    #[test]
    fn nonterminating_action_cycle_is_bounded() {
        let mut to2 = action(0x81, &[1, 0]);
        to2.push(0);
        let mut to1 = action(0x81, &[0, 0]);
        to1.push(0);
        let (bytes, swf) = movie(vec![(1, vec![actions(&to2), actions(&to1)])]);
        let error = timeline::normalize(&bytes, &swf, &BTreeMap::new(), &[request()])
            .err()
            .unwrap();
        assert!(format!("{error:#}").contains("work limit"));
    }

    #[test]
    fn float_and_word_swapped_double_targets() {
        let mut operands = vec![1];
        operands.extend(3f32.to_le_bytes());
        let double = 4f64.to_le_bytes();
        operands.push(6);
        operands.extend(&double[4..]);
        operands.extend(&double[..4]);
        let mut bytes = action(0x96, &operands);
        bytes.extend(action(0x9f, &[0]));
        bytes.extend(action(0x9f, &[1]));
        bytes.push(0);
        assert_eq!(
            decode(&bytes).unwrap(),
            vec![
                Control::Goto {
                    frame: 4,
                    play: false
                },
                Control::Goto {
                    frame: 3,
                    play: true
                }
            ]
        );
    }
    #[test]
    fn initializer_and_as3_actions_are_not_reclassified() {
        let (bytes, parsed) = movie(vec![(
            1,
            vec![[actions(&[7, 0]), tag(59, &[1, 0, 7, 0])].concat()],
        )]);
        assert!(timeline::normalize(&bytes, &parsed, &BTreeMap::new(), &[request()]).is_err());
        let (bytes, _) = movie(vec![(1, vec![actions(&[7, 0])])]);
        let mut as3 = bytes[..13].to_vec();
        as3.extend(tag(69, &[8, 0, 0, 0]));
        as3.extend(&bytes[13..]);
        let len = as3.len() as u32;
        as3[4..8].copy_from_slice(&len.to_le_bytes());
        let parsed = Swf::parse(&as3).unwrap();
        assert!(prepare(
            &as3,
            &parsed,
            &BTreeMap::new(),
            &[request()],
            &BTreeSet::new()
        )
        .unwrap()
        .is_none());
        assert!(timeline::normalize(&as3, &parsed, &BTreeMap::new(), &[request()]).is_err());
        let mut initialized = bytes[..13].to_vec();
        initialized.extend(tag(59, &[1, 0, 7, 0]));
        initialized.extend(&bytes[13..]);
        let len = initialized.len() as u32;
        initialized[4..8].copy_from_slice(&len.to_le_bytes());
        let parsed = Swf::parse(&initialized).unwrap();
        let error = timeline::normalize(&initialized, &parsed, &BTreeMap::new(), &[request()])
            .err()
            .unwrap();
        assert!(format!("{error:#}").contains("DoInitAction"));
    }
    #[test]
    fn missing_label_only_callback_keeps_default_playback() {
        let result = normalized(vec![(
            1,
            vec![
                place(101, 1),
                [place(102, 1), goto("missing")].concat(),
                place(103, 1),
            ],
        )]);
        assert_eq!(
            result.decisions[0].selection,
            Selection::Sequence {
                frames: vec![1, 2, 3],
                loop_start: Some(0)
            }
        );
        assert!(result
            .warnings
            .iter()
            .any(|s| s.contains("unknown frame label")));
    }
}
