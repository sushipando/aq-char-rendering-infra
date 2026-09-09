//! Lossless display-list snapshots used when a scripted timeline skips frames.
//! Merge placement fields before writing deltas; surviving children retain their
//! identities and clocks, even across backward jumps.
use crate::swf;
use anyhow::{ensure, Context, Result};
use std::collections::BTreeMap;
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Placement {
    pub generation: usize,
    pub id: u16,
    fields: BTreeMap<u8, Vec<u8>>,
}
pub type Display = BTreeMap<u16, Placement>;
fn take(data: &[u8], at: &mut usize, len: usize) -> Result<Vec<u8>> {
    let v = data
        .get(*at..*at + len)
        .context("truncated display field")?
        .to_vec();
    *at += len;
    Ok(v)
}
fn filters(data: &[u8], at: &mut usize) -> Result<Vec<u8>> {
    let start = *at;
    let count = take(data, at, 1)?[0];
    for _ in 0..count {
        let kind = take(data, at, 1)?[0];
        let n = match kind {
            0 => 23,
            1 => 9,
            2 => 15,
            3 => 27,
            4 | 7 => {
                let n = take(data, at, 1)?[0] as usize;
                5 * n + 19
            }
            5 => {
                let size = take(data, at, 2)?;
                13 + 4 * size[0] as usize * size[1] as usize
            }
            6 => 80,
            _ => anyhow::bail!("unknown SWF filter {kind}"),
        };
        take(data, at, n)?;
    }
    Ok(data[start..*at].to_vec())
}
fn fields(code: u16, data: &[u8]) -> Result<(u16, bool, BTreeMap<u8, Vec<u8>>)> {
    ensure!(
        matches!(code, 26 | 70),
        "scripted frame sequencing needs PlaceObject2/3"
    );
    let flags = *data.first().context("missing placement flags")?;
    let flags2 = if code == 70 {
        *data.get(1).context("missing placement flags")?
    } else {
        0
    };
    ensure!(
        flags & 128 == 0 && flags2 & 0x18 == 0,
        "clip actions/class placements require runtime support"
    );
    let depth = swf::u16_at(data, if code == 70 { 2 } else { 1 })?;
    let mut at = if code == 70 { 4 } else { 3 };
    let mut fields = BTreeMap::new();
    if flags & 2 != 0 {
        fields.insert(0, take(data, &mut at, 2)?);
    }
    if flags & 4 != 0 {
        let end = swf::skip_matrix(data, at)?;
        let len = end - at;
        fields.insert(1, take(data, &mut at, len)?);
    }
    if flags & 8 != 0 {
        let end = swf::skip_color(data, at)?;
        let len = end - at;
        fields.insert(2, take(data, &mut at, len)?);
    }
    if flags & 16 != 0 {
        fields.insert(3, take(data, &mut at, 2)?);
    }
    if flags & 32 != 0 {
        let start = at;
        swf::cstring(data, &mut at)?;
        fields.insert(4, data[start..at].to_vec());
    }
    if flags & 64 != 0 {
        fields.insert(5, take(data, &mut at, 2)?);
    }
    if flags2 & 1 != 0 {
        fields.insert(6, filters(data, &mut at)?);
    }
    for (flag, key, len) in [(2, 7, 1), (4, 8, 1), (32, 9, 1), (64, 10, 4)] {
        if flags2 & flag != 0 {
            fields.insert(key, take(data, &mut at, len)?);
        }
    }
    ensure!(at == data.len(), "unparsed placement fields");
    Ok((depth, flags & 1 != 0, fields))
}
pub fn snapshots(payload: &[u8]) -> Result<Vec<Display>> {
    let mut display = Display::new();
    let mut frames = Vec::new();
    for (generation, (code, data)) in swf::tags(payload, 4)?.into_iter().enumerate() {
        match code {
            26 | 70 => {
                let (depth, moving, fields) = fields(code, data)?;
                let character = fields.get(&0).map(|v| swf::u16_at(v, 0)).transpose()?;
                if moving {
                    let p = display
                        .get_mut(&depth)
                        .context("move of absent display object")?;
                    if let Some(id) = character {
                        p.id = id;
                        p.generation = generation;
                    }
                    p.fields.extend(fields);
                } else {
                    let id = character.context("new placement without character")?;
                    display.insert(
                        depth,
                        Placement {
                            generation,
                            id,
                            fields,
                        },
                    );
                }
            }
            4 => anyhow::bail!("legacy placement in sequenced timeline"),
            5 => {
                display.remove(&swf::u16_at(data, 2)?);
            }
            28 => {
                display.remove(&swf::u16_at(data, 0)?);
            }
            1 => frames.push(display.clone()),
            0 | 43 | 15 | 18 | 19 | 45 | 89 => (), // Labels and audio have no image output.
            12 | 59 => anyhow::bail!("AVM1 actions in sequenced timeline"),
            _ => anyhow::bail!("unsupported sequenced timeline tag {code}"),
        }
        ensure!(
            frames.len().saturating_mul(display.len()) < 2_000_000,
            "display snapshot work limit"
        );
    }
    Ok(frames)
}
impl Placement {
    fn write(&self, out: &mut Vec<u8>, depth: u16, moving: bool, previous: Option<&Self>) {
        let mut flags = if moving { 1 } else { 2 };
        let mut flags2 = 0;
        let mut payload = Vec::new();
        if !moving {
            payload.extend(self.id.to_le_bytes());
        }
        let mut fields = self.fields.clone();
        if moving {
            if let Some(previous) = previous {
                for key in previous.fields.keys() {
                    if fields.contains_key(key) || *key == 0 {
                        continue;
                    }
                    fields.insert(
                        *key,
                        match key {
                            1 | 2 => vec![0],
                            3 | 5 => vec![0, 0],
                            4 | 6 | 8 => vec![0],
                            7 | 9 => vec![1],
                            10 => vec![0, 0, 0, 0],
                            _ => unreachable!(),
                        },
                    );
                }
            }
        }
        for (key, data) in &fields {
            if *key == 0 {
                continue;
            }
            match key {
                1 => flags |= 4,
                2 => flags |= 8,
                3 => flags |= 16,
                4 => flags |= 32,
                5 => flags |= 64,
                6 => flags2 |= 1,
                7 => flags2 |= 2,
                8 => flags2 |= 4,
                9 => flags2 |= 32,
                10 => flags2 |= 64,
                _ => unreachable!(),
            };
            payload.extend(data);
        }
        let mut data = vec![flags, flags2];
        data.extend(depth.to_le_bytes());
        data.extend(payload);
        swf::write_tag(out, 70, &data);
    }
    pub fn apply_flat_layer(&mut self, property: &Self) {
        for key in [2, 6, 7] {
            if let Some(v) = property.fields.get(&key) {
                self.fields.insert(key, v.clone());
            } else {
                self.fields.remove(&key);
            }
        }
        // Generated projection at z=layerDepth=0, with no camera/mask.
        self.fields.insert(1, vec![0]);
        self.set_visible(true);
    }
    pub fn identity_color(&self) -> Result<bool> {
        Ok(self
            .fields
            .get(&2)
            .map(|v| swf::color_transform(v, 0, true))
            .transpose()?
            .is_none_or(|v| v == [256, 256, 256, 256, 0, 0, 0, 0]))
    }
    pub fn masks_following(&self, depth: u16) -> Result<bool> {
        Ok(self
            .fields
            .get(&5)
            .map(|v| swf::u16_at(v, 0))
            .transpose()?
            .is_some_and(|n| n > depth))
    }
    pub fn reset_matrix(&mut self) {
        self.fields.insert(1, vec![0]);
    }
    pub fn set_visible(&mut self, visible: bool) {
        self.fields.insert(9, vec![u8::from(visible)]);
    }
    pub fn name(&self) -> Option<String> {
        self.fields
            .get(&4)
            .and_then(|v| swf::cstring(v, &mut 0).ok())
    }
    pub fn copy_effects(&mut self, other: &Self) {
        for key in [2, 6, 7, 8, 9, 10] {
            if let Some(v) = other.fields.get(&key) {
                self.fields.insert(key, v.clone());
            } else {
                self.fields.remove(&key);
            }
        }
    }
    pub fn identity_matrix(&self) -> bool {
        self.fields.get(&1).is_none_or(|v| v == &[0])
    }
    pub fn effects(&self) -> Vec<(u8, Vec<u8>)> {
        self.fields
            .iter()
            .filter(|(k, _)| matches!(k, 2 | 6 | 7 | 8 | 9 | 10))
            .map(|(k, v)| (*k, v.clone()))
            .collect()
    }
}
pub fn write_frames(id: u16, frames: &[Display]) -> Result<Vec<u8>> {
    ensure!(!frames.is_empty(), "empty display sequence");
    let mut out = id.to_le_bytes().to_vec();
    out.extend(u16::try_from(frames.len())?.to_le_bytes());
    let mut previous = Display::new();
    for current in frames {
        for (depth, old) in &previous {
            if current
                .get(depth)
                .is_none_or(|p| p.id != old.id || p.generation != old.generation)
            {
                swf::write_tag(&mut out, 28, &depth.to_le_bytes());
            }
        }
        for (depth, p) in current {
            let old = previous.get(depth);
            if old != Some(p) {
                p.write(
                    &mut out,
                    *depth,
                    old.is_some_and(|v| v.id == p.id && v.generation == p.generation),
                    old,
                );
            }
        }
        swf::write_tag(&mut out, 1, &[]);
        previous = current.clone();
    }
    swf::write_tag(&mut out, 0, &[]);
    Ok(out)
}
pub fn sequence(payload: &[u8], indices: &[usize], pad_to: usize) -> Result<Vec<u8>> {
    let original = snapshots(payload)?;
    let mut selected = indices
        .iter()
        .map(|i| {
            original
                .get(i - 1)
                .cloned()
                .context("frame outside source sequence")
        })
        .collect::<Result<Vec<_>>>()?;
    if selected.len() < pad_to {
        let last = selected.last().cloned().context("empty frame sequence")?;
        selected.resize(pad_to, last);
    }
    write_frames(swf::u16_at(payload, 0)?, &selected)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn backward_jump_clears_effects_without_recreating_surviving_child() {
        let plain = Placement {
            generation: 4,
            id: 22,
            fields: BTreeMap::from([(1, vec![0]), (4, b"child\0".to_vec())]),
        };
        let mut blurred = plain.clone();
        let mut filter = vec![1, 1];
        filter.extend([0; 9]);
        blurred.fields.insert(6, filter);
        let encoded = write_frames(
            7,
            &[
                BTreeMap::from([(1, plain.clone())]),
                BTreeMap::from([(1, blurred)]),
                BTreeMap::from([(1, plain)]),
            ],
        )
        .unwrap();
        let tags = swf::tags(&encoded, 4).unwrap();
        assert!(!tags.iter().any(|(c, _)| *c == 28));
        let frames = snapshots(&encoded).unwrap();
        assert_eq!(frames[0][&1].generation, frames[2][&1].generation);
        assert_eq!(frames[2][&1].fields[&6], vec![0]);
    }
    #[test]
    fn replacement_with_same_character_restarts_instance() {
        let first = Placement {
            generation: 1,
            id: 22,
            fields: BTreeMap::new(),
        };
        let mut replacement = first.clone();
        replacement.generation = 2;
        let encoded = write_frames(
            7,
            &[
                BTreeMap::from([(1, first)]),
                BTreeMap::from([(1, replacement)]),
            ],
        )
        .unwrap();
        assert!(swf::tags(&encoded, 4)
            .unwrap()
            .iter()
            .any(|(c, _)| *c == 28));
    }
}
