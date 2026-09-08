//! Preserve the complete background stage as a nested sprite for ordinary export.
use anyhow::{ensure, Context, Result};
use serde_json::Value;

pub const KEY: &str = "presentation_background";
pub const POLICY: &str = "background-timeline-v1";

pub fn record(index: usize) -> Option<Value> {
    serde_json::from_slice::<Vec<Value>>(include_bytes!("../assets/charpage/sources.json"))
        .expect("background catalog")
        .into_iter()
        .find(|v| v["index"] == index)
}

fn tag(out: &mut Vec<u8>, code: u16, data: &[u8]) {
    out.extend_from_slice(&((code << 6) | 63).to_le_bytes());
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(data);
}

/// A one-frame wrapper lets FFDec's sublength exporter advance both the original
/// root timeline and all children together. Keep root placement transforms and
/// map a document class to the nested stage so script validation still applies.
pub fn wrap(bytes: &[u8]) -> Result<(Vec<u8>, u16, Option<usize>)> {
    let body = crate::swf::decompress(bytes)?;
    let header = (5 + 4 * (body[0] as usize >> 3)).div_ceil(8) + 4;
    let frames = crate::swf::u16_at(&body, header - 2)?;
    let tags = crate::swf::tags(&body, header)?;
    let used: std::collections::BTreeSet<_> = tags
        .iter()
        .filter_map(|(_, data)| crate::swf::u16_at(data, 0).ok())
        .collect();
    let ids: Vec<_> = (1..=u16::MAX)
        .rev()
        .filter(|id| !used.contains(id))
        .take(2)
        .collect();
    ensure!(ids.len() == 2, "no background wrapper IDs");
    let (stage, wrapper) = (ids[0], ids[1]);
    let mut movie = body[..header].to_vec();
    movie[header - 2..].copy_from_slice(&1u16.to_le_bytes());
    let mut timeline = Vec::new();
    timeline.extend_from_slice(&stage.to_le_bytes());
    timeline.extend_from_slice(&frames.to_le_bytes());
    for (code, data) in tags {
        match code {
            0 => {}
            1 | 4 | 5 | 12 | 15 | 18 | 19 | 26 | 28 | 43 | 45 | 61 | 70 | 89 | 94 => {
                tag(&mut timeline, code, data)
            }
            76 => {
                let mut mapped = data.to_vec();
                let mut at = 2;
                for _ in 0..crate::swf::u16_at(data, 0)? {
                    if crate::swf::u16_at(data, at)? == 0 {
                        mapped[at..at + 2].copy_from_slice(&stage.to_le_bytes());
                    }
                    at += 2;
                    at += data
                        .get(at..)
                        .context("invalid SymbolClass")?
                        .iter()
                        .position(|b| *b == 0)
                        .context("unterminated SymbolClass")?
                        + 1;
                }
                tag(&mut movie, code, &mapped);
            }
            _ => tag(&mut movie, code, data),
        }
    }
    tag(&mut timeline, 0, &[]);
    tag(&mut movie, 39, &timeline);
    let mut outer = Vec::new();
    outer.extend_from_slice(&wrapper.to_le_bytes());
    outer.extend_from_slice(&1u16.to_le_bytes());
    let mut place = vec![6, 1, 0]; // HasCharacter + HasMatrix, depth 1
    place.extend_from_slice(&stage.to_le_bytes());
    place.push(0); // identity MATRIX
    tag(&mut outer, 26, &place);
    tag(&mut outer, 1, &[]);
    tag(&mut outer, 0, &[]);
    tag(&mut movie, 39, &outer);
    tag(&mut movie, 1, &[]);
    tag(&mut movie, 0, &[]);
    let mut out = b"FWS".to_vec();
    out.push(bytes[3]);
    out.extend_from_slice(&((movie.len() + 8) as u32).to_le_bytes());
    out.extend_from_slice(&movie);
    let swf = crate::swf::Swf::parse(&out)?;
    // Conservative authored cycle, including idle spans before a late effect.
    // Never infer that a static prefix means the entire background is static.
    let period = swf.sprites.values().try_fold(1usize, |a, data| {
        crate::swf::u16_at(data, 2)
            .ok()
            .and_then(|b| crate::geometry::lcm(a, b as usize))
    });
    Ok((out, wrapper, period))
}
