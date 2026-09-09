//! Bounded FWS/CWS metadata reader; no FFDec launch for symbol discovery.
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Read,
};

pub(crate) fn u16_at(data: &[u8], offset: usize) -> Result<u16> {
    Ok(u16::from_le_bytes(
        data.get(offset..offset + 2)
            .context("truncated SWF u16")?
            .try_into()?,
    ))
}

fn decode(data: &[u8]) -> Result<(Vec<u8>, bool)> {
    ensure!(data.len() >= 12, "truncated SWF");
    let declared = u32::from_le_bytes(data[4..8].try_into()?) as usize;
    ensure!(
        (12..=128 * 1024 * 1024).contains(&declared),
        "invalid SWF size"
    );
    let mut body = match &data[..3] {
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
    let repair = body.len() + 10 == declared;
    if repair {
        // Only a complete stream ending on a complete final ShowFrame may be
        // missing the two-byte End tag. Never relax arbitrary length mismatches.
        if &data[..3] == b"CWS" {
            let mut decoder = flate2::Decompress::new(true);
            let mut checked = vec![0; body.len() + 1];
            let status = decoder.decompress(&data[8..], &mut checked, flate2::FlushDecompress::Finish)?;
            ensure!(status == flate2::Status::StreamEnd && decoder.total_in() as usize == data.len()-8,
                "incomplete or trailing compressed SWF data");
        }
        let offset = (5 + 4 * (body.first().context("missing SWF stage")? >> 3) as usize).div_ceil(8) + 4;
        let entries = tags(&body, offset)?;
        ensure!(entries.last() == Some(&(1, &[][..])) && !entries.iter().any(|(code,_)| *code == 0)
            && entries.iter().filter(|(code,_)| *code == 1).count() == u16_at(&body, offset-2)? as usize,
            "SWF size mismatch is not a missing final End tag");
        body.extend_from_slice(&[0,0]);
    }
    ensure!(body.len() + 8 == declared, "SWF declared size mismatch");
    Ok((body, repair))
}

pub fn decompress(data: &[u8]) -> Result<Vec<u8>> { Ok(decode(data)?.0) }

/// Preserve valid originals; repair only the bounded missing-End compatibility case.
/// Cached source hashes continue to describe the original bytes.
pub fn repair_missing_end(data: &[u8]) -> Result<Vec<u8>> {
    let (body, repaired) = decode(data)?;
    if !repaired { return Ok(data.to_vec()); }
    let mut result = data[..8].to_vec();
    result[..3].copy_from_slice(b"FWS");
    result.extend(body);
    Ok(result)
}

pub fn tags(data: &[u8], mut offset: usize) -> Result<Vec<(u16, &[u8])>> {
    let mut result = Vec::new();
    while offset < data.len() {
        ensure!(result.len() < 2_000_000, "SWF tag count limit exceeded");
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

pub(crate) fn cstring(data: &[u8], offset: &mut usize) -> Result<String> {
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
                if let Some(stage) = self
                    .symbols
                    .iter()
                    .find(|(_, n)| n == crate::stage_asset::CLASS)
                {
                    return Some(stage.clone());
                }
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

pub(crate) fn skip_matrix(data: &[u8], offset: usize) -> Result<usize> {
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

pub(crate) fn skip_color(data: &[u8], offset: usize) -> Result<usize> {
    let mut bits = Bits {
        data,
        offset: offset * 8,
    };
    let add = bits.unsigned(1)?;
    let mult = bits.unsigned(1)?;
    let n = bits.unsigned(4)? as usize;
    for _ in 0..4 * (add + mult) {
        bits.signed(n)?;
    }
    Ok(bits.bytes())
}

pub(crate) fn color_transform(data: &[u8], offset: usize, alpha: bool) -> Result<[i64; 8]> {
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

/// Placement identity/name only. Rendering fields remain byte-for-byte in the SWF.
pub(crate) fn instance(code: u16, data: &[u8]) -> Result<Option<(u16, Option<u16>, Option<String>, bool)>> {
    if code == 4 {
        return Ok(Some((u16_at(data, 2)?, Some(u16_at(data, 0)?), None, false)));
    }
    if !matches!(code, 26 | 70 | 94) { return Ok(None); }
    let flags = *data.first().context("truncated placement")?;
    ensure!(flags & 128 == 0, "AVM1 placement clip actions are unsupported");
    let (depth, mut offset) = if code == 26 { (u16_at(data, 1)?, 3) } else { (u16_at(data, 2)?, 4) };
    if code != 26 && (data[1] & 8 != 0 || (data[1] & 16 != 0 && flags & 2 != 0)) {
        cstring(data, &mut offset)?;
        ensure!(flags & 2 != 0, "class-only dynamic sprite placement is unsupported");
    }
    let id = if flags & 2 != 0 { let id = u16_at(data, offset)?; offset += 2; Some(id) } else { None };
    if flags & 4 != 0 { offset = skip_matrix(data, offset)?; }
    if flags & 8 != 0 {
        let mut bits = Bits { data, offset: offset * 8 };
        let add = bits.unsigned(1)?;
        let mult = bits.unsigned(1)?;
        let n = bits.unsigned(4)? as usize;
        for _ in 0..4 * (add + mult) { bits.signed(n)?; }
        offset = bits.bytes();
    }
    if flags & 16 != 0 { u16_at(data, offset)?; offset += 2; }
    let name = if flags & 32 != 0 { Some(cstring(data, &mut offset)?) } else { None };
    Ok(Some((depth, id, name, flags & 1 != 0)))
}

/// Strict neutral advanced-layer placement: identity matrix, no color transform,
/// ratio, masking or clip actions. Preserve and compare every remaining effect byte.
pub(crate) fn flat_layer_style(code: u16, data: &[u8]) -> Result<Vec<u8>> {
    ensure!(matches!(code,26|70), "Animate layers: unsupported placement type");
    ensure!(data.first()==Some(&0x26), "Animate layers: non-neutral placement flags");
    let mut offset = if code==26 {5} else {6}; // flags, depth, character
    let flags2 = if code==70 {data[1]} else {0};
    ensure!(flags2 & !7 == 0,"Animate layers: unsupported visibility/3D/class placement");
    ensure!(data.get(offset)==Some(&0),"Animate layers: nonidentity layer matrix");
    offset+=1;
    cstring(data,&mut offset)?;
    let mut signature = vec![flags2];
    signature.extend_from_slice(&data[offset..]);
    Ok(signature)
}

pub(crate) fn write_tag(output: &mut Vec<u8>, code: u16, data: &[u8]) {
    let short = data.len().min(63) as u16;
    output.extend_from_slice(&(code << 6 | short).to_le_bytes());
    if short == 63 { output.extend_from_slice(&(data.len() as u32).to_le_bytes()); }
    output.extend_from_slice(data);
}

/// Replace only DefineSprite payloads, preserving the stage header and all other tags.
pub(crate) fn replace_sprites(source: &[u8], replacements: &BTreeMap<u16, Vec<u8>>) -> Result<Vec<u8>> {
    if replacements.is_empty() { return repair_missing_end(source); }
    let body = decompress(source)?;
    let offset = (5 + 4 * (body[0] as usize >> 3)).div_ceil(8) + 4;
    let mut result = Vec::from(&source[..8]);
    result[..3].copy_from_slice(b"FWS");
    result.extend_from_slice(body.get(..offset).context("truncated SWF stage")?);
    for (code, data) in tags(&body, offset)? {
        let replacement = if code == 39 { replacements.get(&u16_at(data, 0)?) } else { None };
        write_tag(&mut result, code, replacement.map(Vec::as_slice).unwrap_or(data));
    }
    let size = u32::try_from(result.len())?;
    result[4..8].copy_from_slice(&size.to_le_bytes());
    Ok(result)
}

pub(crate) fn set_placed_id(code: u16, data: &mut [u8], id: u16) -> Result<()> {
    let mut offset = match code {
        4 => 0,
        26 => 3,
        70 | 94 => 4,
        _ => anyhow::bail!("not a placement"),
    };
    if code != 4 {
        ensure!(data[0] & 2 != 0, "placement has no character");
        if code != 26 && (data[1] & 8 != 0 || data[1] & 16 != 0 && data[0] & 2 != 0) {
            cstring(data, &mut offset)?;
        }
    }
    data.get_mut(offset..offset + 2)
        .context("truncated placed ID")?
        .copy_from_slice(&id.to_le_bytes());
    Ok(())
}

pub(crate) fn replace_dictionary(
    source: &[u8],
    replacements: &BTreeMap<u16, Vec<u8>>,
    symbols: &[(u16, String)],
) -> Result<Vec<u8>> {
    let body = decompress(source)?;
    let offset = (5 + 4 * (body[0] as usize >> 3)).div_ceil(8) + 4;
    let mut result = source[..8].to_vec();
    result[..3].copy_from_slice(b"FWS");
    result.extend_from_slice(&body[..offset]);
    let mut originals = BTreeSet::new();
    let mut tail = Vec::new();
    for (code, data) in tags(&body, offset)? {
        if code == 76 {
            continue;
        }
        if matches!(
            code,
            0 | 1 | 4 | 5 | 12 | 15 | 18 | 19 | 26 | 28 | 43 | 45 | 61 | 70 | 89 | 94
        ) {
            write_tag(&mut tail, code, data);
            continue;
        }
        if code == 39 {
            let id = u16_at(data, 0)?;
            originals.insert(id);
            write_tag(
                &mut result,
                code,
                replacements.get(&id).map(Vec::as_slice).unwrap_or(data),
            );
        } else {
            write_tag(&mut result, code, data);
        }
    }
    for (id, payload) in replacements {
        if !originals.contains(id) {
            write_tag(&mut result, 39, payload);
        }
    }
    if !symbols.is_empty() {
        let mut names = u16::try_from(symbols.len())?.to_le_bytes().to_vec();
        for (id, name) in symbols {
            names.extend(id.to_le_bytes());
            names.extend(name.as_bytes());
            names.push(0);
        }
        write_tag(&mut result, 76, &names);
    }
    result.extend(tail);
    let len = u32::try_from(result.len())?;
    result[4..8].copy_from_slice(&len.to_le_bytes());
    Ok(result)
}

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
    #[test]
    fn repair_only_complete_stream_missing_final_end() {
        use std::io::Write;
        let body = vec![0,0,24,1,0,64,0]; // stage + one complete ShowFrame
        let make = |body: &[u8], compressed: bool, declared: usize| {
            let mut bytes = if compressed {b"CWS\x09".to_vec()} else {b"FWS\x09".to_vec()};
            bytes.extend_from_slice(&(declared as u32).to_le_bytes());
            if compressed {
                let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(),flate2::Compression::default());
                encoder.write_all(body).unwrap();
                bytes.extend(encoder.finish().unwrap());
            } else {bytes.extend(body);}
            bytes
        };
        for compressed in [false,true] {
            let bytes = make(&body,compressed,body.len()+10);
            let repaired = super::repair_missing_end(&bytes).unwrap();
            assert_eq!(&repaired[..3],b"FWS");
            assert_eq!(&repaired[8..body.len()+8],body.as_slice());
            assert_eq!(&repaired[repaired.len()-2..],&[0,0]);
            assert_eq!(super::repair_missing_end(&repaired).unwrap(),repaired);
            assert!(super::decompress(&make(&body,compressed,body.len()+11)).is_err());
            let mut wrong_frames = body.clone(); wrong_frames[3]=2;
            assert!(super::decompress(&make(&wrong_frames,compressed,body.len()+10)).is_err());
            let mut ended = body.clone(); ended.extend([0,0]);
            assert!(super::decompress(&make(&ended,compressed,ended.len()+10)).is_err());
            let mut truncated_tag = body.clone(); truncated_tag.extend([0x85,0,1]);
            assert!(super::decompress(&make(&truncated_tag,compressed,truncated_tag.len()+10)).is_err());
        }
        let mut incomplete_stream = make(&body,true,body.len()+10);
        incomplete_stream.truncate(incomplete_stream.len()-2);
        assert!(super::decompress(&incomplete_stream).is_err());
    }

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
            assert_eq!(swf.timeline(1).unwrap(), (3, 1));
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

/// Some exporters write zero-frame, unbound sprites containing a complete
/// initial display list. Flash constructs that display list once. Supply the
/// implicit ShowFrame required by FFDec's SVG exporter; never invent callbacks
/// for a class-bound clip or repair truncated/action-bearing payloads.
pub(crate) fn repair_zero_frame_displays(source: &[u8], swf: &Swf) -> Result<Option<Vec<u8>>> {
    let mut replacements = BTreeMap::new();
    for (id, payload) in &swf.sprites {
        if u16_at(payload, 2)? != 0 || swf.symbols.iter().any(|(n, _)| n == id) {
            continue;
        }
        let tags = tags(payload, 4)?;
        if tags.last() != Some(&(0, &[][..]))
            || !tags
                .iter()
                .all(|(code, _)| matches!(code, 0 | 4 | 5 | 26 | 28 | 70))
        {
            continue;
        }
        let mut out = id.to_le_bytes().to_vec();
        out.extend(1u16.to_le_bytes());
        for (code, data) in tags {
            if code == 0 {
                continue;
            }
            instance(code, data)?;
            write_tag(&mut out, code, data);
        }
        write_tag(&mut out, 1, &[]);
        write_tag(&mut out, 0, &[]);
        replacements.insert(*id, out);
    }
    if replacements.is_empty() {
        Ok(None)
    } else {
        Ok(Some(replace_sprites(source, &replacements)?))
    }
}
