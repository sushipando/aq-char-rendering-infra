//! Bounded FWS/CWS metadata reader; no FFDec launch for symbol discovery.
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Read,
};

fn u16_at(data: &[u8], offset: usize) -> Result<u16> {
    Ok(u16::from_le_bytes(
        data.get(offset..offset + 2)
            .context("truncated SWF u16")?
            .try_into()?,
    ))
}

pub fn decompress(data: &[u8]) -> Result<Vec<u8>> {
    ensure!(data.len() >= 12, "truncated SWF");
    let declared = u32::from_le_bytes(data[4..8].try_into()?) as usize;
    ensure!(
        (12..=128 * 1024 * 1024).contains(&declared),
        "invalid SWF size"
    );
    let body = match &data[..3] {
        b"FWS" => data[8..].to_vec(),
        b"CWS" => {
            let mut output = Vec::new();
            flate2::read::ZlibDecoder::new(&data[8..])
                .take((declared + 1) as u64)
                .read_to_end(&mut output)?;
            output
        }
        _ => bail!("only FWS/CWS sources are supported"),
    };
    ensure!(body.len() + 8 == declared, "SWF declared size mismatch");
    Ok(body)
}

pub fn tags(data: &[u8], mut offset: usize) -> Result<Vec<(u16, &[u8])>> {
    let mut result = Vec::new();
    while offset < data.len() {
        let header = u16_at(data, offset)?;
        offset += 2;
        let mut length = (header & 63) as usize;
        if length == 63 {
            length = u32::from_le_bytes(
                data.get(offset..offset + 4)
                    .context("truncated SWF tag length")?
                    .try_into()?,
            ) as usize;
            offset += 4;
        }
        let end = offset.checked_add(length).context("SWF tag overflow")?;
        result.push((
            header >> 6,
            data.get(offset..end).context("truncated SWF tag")?,
        ));
        offset = end;
        if header >> 6 == 0 {
            break;
        }
    }
    Ok(result)
}

fn cstring(data: &[u8], offset: &mut usize) -> Result<String> {
    let end = data
        .get(*offset..)
        .context("truncated SWF string")?
        .iter()
        .position(|b| *b == 0)
        .context("unterminated SWF string")?
        + *offset;
    let result = String::from_utf8_lossy(&data[*offset..end]).into_owned();
    *offset = end + 1;
    Ok(result)
}

pub struct Swf {
    pub frame_rate: f64,
    pub symbols: Vec<(u16, String)>,
    pub sprites: BTreeMap<u16, Vec<u8>>,
}

impl Swf {
    pub fn parse(data: &[u8]) -> Result<Self> {
        let body = decompress(data)?;
        let rect_size = (5 + 4 * (body[0] as usize >> 3)).div_ceil(8);
        let frame_rate = u16_at(&body, rect_size)? as f64 / 256.0;
        ensure!(
            frame_rate > 0.0 && frame_rate <= 1000.0,
            "invalid SWF frame rate"
        );
        let mut value = Self {
            frame_rate,
            symbols: Vec::new(),
            sprites: BTreeMap::new(),
        };
        for (code, payload) in tags(&body, rect_size + 4)? {
            match code {
                39 => {
                    ensure!(payload.len() >= 4, "truncated sprite");
                    value.sprites.insert(u16_at(payload, 0)?, payload.to_vec());
                }
                76 => {
                    let mut offset = 2;
                    for _ in 0..u16_at(payload, 0)? {
                        let id = u16_at(payload, offset)?;
                        offset += 2;
                        value.symbols.push((id, cstring(payload, &mut offset)?));
                    }
                }
                _ => (),
            }
        }
        Ok(value)
    }

    pub fn symbol(&self, name: &str) -> Option<(u16, String)> {
        self.symbols
            .iter()
            .find(|(_, n)| n.eq_ignore_ascii_case(name))
            .cloned()
            .or_else(|| {
                if !name.is_empty() {
                    return None;
                }
                if self.symbols.len() == 1 {
                    return self.symbols.first().cloned();
                }
                self.sprites.last_key_value().map(|(id, _)| {
                    (
                        *id,
                        self.symbols
                            .iter()
                            .find(|(n, _)| n == id)
                            .map(|(_, n)| n.clone())
                            .unwrap_or_else(|| "source".into()),
                    )
                })
            })
    }

    pub fn timeline(&self, id: u16) -> Result<(usize, usize)> {
        let payload = self
            .sprites
            .get(&id)
            .context("requested symbol is not a sprite")?;
        let count = u16_at(payload, 2)? as usize;
        ensure!(count > 0, "empty root timeline");
        let mut idle = None;
        let mut ready = None;
        let mut frame = 1;
        for (code, value) in tags(payload, 4)? {
            if code == 1 {
                frame += 1;
            }
            if code == 43 {
                let label = cstring(value, &mut 0)?.to_lowercase();
                if matches!(label.as_str(), "idle" | "idel" | "id") && idle.is_none() {
                    idle = Some(frame);
                }
                if label == "ready" && ready.is_none() {
                    ready = Some(frame);
                }
            }
        }
        let selected = match (idle, ready) {
            (Some(i), Some(r)) if r < i => i - 1,
            (Some(i), _) => i,
            (_, Some(r)) => r,
            _ => 1,
        };
        ensure!(selected <= count, "frame label exceeds timeline");
        Ok((
            selected,
            if idle.is_some() || ready.is_some() {
                1
            } else {
                count
            },
        ))
    }

    pub fn placement_colors(&self) -> Result<BTreeMap<String, Value>> {
        let mut observed: BTreeMap<(u16, u16), BTreeSet<[i64; 8]>> = BTreeMap::new();
        let identity = [256, 256, 256, 256, 0, 0, 0, 0];
        for (parent, sprite) in &self.sprites {
            let mut display = BTreeMap::new();
            for (code, data) in tags(sprite, 4)? {
                if let Some((depth, id, color)) = placement(code, data)? {
                    let effective = id.or_else(|| display.get(&depth).copied());
                    if let Some(id) = id {
                        display.insert(depth, id);
                        if color.is_none() {
                            observed.entry((*parent, id)).or_default().insert(identity);
                        }
                    }
                    if let (Some(id), Some(color)) = (effective, color) {
                        observed.entry((*parent, id)).or_default().insert(color);
                    }
                } else if code == 5 {
                    display.remove(&u16_at(data, 2)?);
                } else if code == 28 {
                    display.remove(&u16_at(data, 0)?);
                }
            }
        }
        Ok(observed.into_iter().filter_map(|((parent,child), colors)| {
            if colors.len() != 1 { return None; }
            let c = colors.into_iter().next().unwrap();
            (c != identity).then(|| (format!("{parent},{child}"),json!({"red_mult":c[0],"green_mult":c[1],"blue_mult":c[2],"alpha_mult":c[3],"red_add":c[4],"green_add":c[5],"blue_add":c[6],"alpha_add":c[7]})))
        }).collect())
    }
}

struct Bits<'a> {
    data: &'a [u8],
    offset: usize,
}
impl Bits<'_> {
    fn unsigned(&mut self, count: usize) -> Result<u32> {
        ensure!(
            count <= 32 && self.offset + count <= self.data.len() * 8,
            "truncated SWF bit field"
        );
        let mut value = 0;
        for _ in 0..count {
            value =
                (value << 1) | ((self.data[self.offset / 8] >> (7 - self.offset % 8)) & 1) as u32;
            self.offset += 1;
        }
        Ok(value)
    }
    fn signed(&mut self, count: usize) -> Result<i64> {
        let value = self.unsigned(count)? as i64;
        Ok(if count > 0 && value & (1 << (count - 1)) != 0 {
            value - (1_i64 << count)
        } else {
            value
        })
    }
    fn bytes(&self) -> usize {
        self.offset.div_ceil(8)
    }
}

fn skip_matrix(data: &[u8], offset: usize) -> Result<usize> {
    let mut bits = Bits {
        data,
        offset: offset * 8,
    };
    for _ in 0..2 {
        if bits.unsigned(1)? != 0 {
            let n = bits.unsigned(5)? as usize;
            bits.signed(n)?;
            bits.signed(n)?;
        }
    }
    let n = bits.unsigned(5)? as usize;
    bits.signed(n)?;
    bits.signed(n)?;
    Ok(bits.bytes())
}

fn color_transform(data: &[u8], offset: usize, alpha: bool) -> Result<[i64; 8]> {
    let mut bits = Bits {
        data,
        offset: offset * 8,
    };
    let add = bits.unsigned(1)? != 0;
    let mult = bits.unsigned(1)? != 0;
    let n = bits.unsigned(4)? as usize;
    let mut value = [256, 256, 256, 256, 0, 0, 0, 0];
    let channels = if alpha { 4 } else { 3 };
    if mult {
        for channel in value.iter_mut().take(channels) {
            *channel = bits.signed(n)?;
        }
    }
    if add {
        for channel in value.iter_mut().skip(4).take(channels) {
            *channel = bits.signed(n)?;
        }
    }
    Ok(value)
}

type Placement = (u16, Option<u16>, Option<[i64; 8]>);
fn placement(code: u16, data: &[u8]) -> Result<Option<Placement>> {
    if code == 4 {
        let offset = skip_matrix(data, 4)?;
        return Ok(Some((
            u16_at(data, 2)?,
            Some(u16_at(data, 0)?),
            if offset < data.len() {
                Some(color_transform(data, offset, false)?)
            } else {
                None
            },
        )));
    }
    if !matches!(code, 26 | 70 | 94) {
        return Ok(None);
    }
    let flags = *data.first().context("truncated placement")?;
    let (depth, mut offset) = if code == 26 {
        (u16_at(data, 1)?, 3)
    } else {
        (u16_at(data, 2)?, 4)
    };
    if code != 26 {
        let flags2 = data[1];
        if flags2 & 8 != 0 || (flags2 & 16 != 0 && flags & 2 != 0) {
            cstring(data, &mut offset)?;
        }
    }
    let id = if flags & 2 != 0 {
        let id = u16_at(data, offset)?;
        offset += 2;
        Some(id)
    } else {
        None
    };
    if flags & 4 != 0 {
        offset = skip_matrix(data, offset)?;
    }
    let color = if flags & 8 != 0 {
        Some(color_transform(data, offset, true)?)
    } else {
        None
    };
    Ok(Some((depth, id, color)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tag(code: u16, data: &[u8]) -> Vec<u8> {
        assert!(data.len() < 63);
        [
            (code << 6 | data.len() as u16).to_le_bytes().as_slice(),
            data,
        ]
        .concat()
    }
    fn source() -> Vec<u8> {
        let mut sprite = vec![1, 0, 3, 0];
        sprite.extend(tag(43, b"Ready\0"));
        sprite.extend(tag(1, &[]));
        sprite.extend(tag(1, &[]));
        sprite.extend(tag(43, b"Idle\0"));
        sprite.extend(tag(1, &[]));
        sprite.extend(tag(0, &[]));
        let mut body = vec![0, 0, 24, 1, 0]; // empty RECT, FIXED8 24fps, 1 root frame
        body.extend(tag(39, &sprite));
        body.extend(tag(76, b"\x01\x00\x01\x00Example\0"));
        body.extend(tag(0, &[]));
        [
            b"FWS\x0a".as_slice(),
            &((body.len() + 8) as u32).to_le_bytes(),
            &body,
        ]
        .concat()
    }

    #[test]
    fn fws_cws_symbol_and_ready_idle_selection_agree() {
        let fws = source();
        let mut encoder =
            flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(&fws[8..]).unwrap();
        let mut cws = fws[..8].to_vec();
        cws[0] = b'C';
        cws.extend(encoder.finish().unwrap());
        for bytes in [fws, cws] {
            let swf = Swf::parse(&bytes).unwrap();
            assert_eq!(swf.frame_rate, 24.0);
            assert_eq!(swf.symbol("example").unwrap().0, 1);
            assert_eq!(swf.timeline(1).unwrap(), (2, 1));
            assert!(Swf::parse(&bytes[..bytes.len() - 1]).is_err());
        }
    }

    #[test]
    fn unlabeled_root_advances_and_ambiguous_cxform_is_not_guessed() {
        let mut payload = vec![1, 0, 2, 0];
        // PlaceObject2: child 2, depth 1, alpha-aware all-zero multipliers.
        payload.extend(tag(26, &[0x0a, 1, 0, 2, 0, 0x40]));
        payload.extend(tag(1, &[]));
        let mut swf = Swf {
            frame_rate: 24.0,
            symbols: vec![],
            sprites: BTreeMap::from([(1, payload)]),
        };
        assert_eq!(swf.timeline(1).unwrap(), (1, 2));
        assert_eq!(swf.placement_colors().unwrap()["1,2"]["alpha_mult"], 0);
        // A second placement of the same child with identity is ambiguous.
        swf.sprites
            .get_mut(&1)
            .unwrap()
            .extend(tag(26, &[2, 1, 0, 2, 0]));
        assert!(swf.placement_colors().unwrap().is_empty());
    }

    #[test]
    fn malformed_lengths_and_bits_fail_without_panicking() {
        assert!(tags(&[0x7f, 0], 0).is_err());
        assert!(placement(26, &[0x0e, 1, 0, 2, 0]).is_err());
        let mut bytes = source();
        bytes[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(decompress(&bytes).is_err());
    }
}
