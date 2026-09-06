//! Container-only checks and the single-frame animation case. No pixel encoding.
use anyhow::{ensure, Context, Result};

pub const MAX_DURATION: u32 = 0x00ff_ffff;
/// Full replacement, leave the result on the canvas (`+0-b` in webpmux).
pub const REPLACE_NO_DISPOSE: u8 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameInfo {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    pub duration: u32,
    pub flags: u8,
}

pub struct WebpInfo {
    pub canvas: [u32; 2],
    pub frames: Vec<FrameInfo>,
    pub loop_count: Option<u16>,
    pub background: Option<u32>,
    pub has_metadata: bool,
    flags: u8,
}

struct Chunk<'a> {
    kind: &'a [u8],
    data: &'a [u8],
}

fn uint24(data: &[u8]) -> Result<u32> {
    ensure!(data.len() >= 3, "truncated WebP integer");
    Ok(data[0] as u32 | (data[1] as u32) << 8 | (data[2] as u32) << 16)
}

fn chunks(mut data: &[u8]) -> Result<Vec<Chunk<'_>>> {
    let mut result = Vec::new();
    while !data.is_empty() {
        ensure!(result.len() < 10_000, "WebP chunk count limit exceeded");
        let header = data.get(..8).context("truncated WebP chunk")?;
        let length = u32::from_le_bytes(header[4..8].try_into()?) as usize;
        let end = 8usize.checked_add(length).context("WebP chunk overflow")?;
        let padded = end
            .checked_add(length & 1)
            .context("WebP padding overflow")?;
        let payload = data.get(8..end).context("truncated WebP payload")?;
        if length & 1 != 0 {
            ensure!(data.get(end) == Some(&0), "invalid WebP chunk padding");
        }
        result.push(Chunk {
            kind: &header[..4],
            data: payload,
        });
        data = data.get(padded..).context("truncated WebP padding")?;
    }
    Ok(result)
}

fn container(bytes: &[u8]) -> Result<Vec<Chunk<'_>>> {
    ensure!(
        bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP",
        "not a WebP container"
    );
    ensure!(
        u32::from_le_bytes(bytes[4..8].try_into()?) as u64 + 8 == bytes.len() as u64,
        "WebP RIFF size mismatch"
    );
    chunks(&bytes[12..])
}

/// Check the encoded bitstream dimensions, not only an untrusted VP8X header.
fn image_size(chunks: &[Chunk<'_>]) -> Result<([u32; 2], bool)> {
    let mut size = None;
    let mut alpha = false;
    for chunk in chunks {
        let payload = chunk.data;
        let value = match chunk.kind {
            b"ALPH" => {
                ensure!(
                    !alpha && size.is_none() && !payload.is_empty(),
                    "invalid ALPH ordering"
                );
                alpha = true;
                continue;
            }
            b"VP8L" => {
                ensure!(
                    !alpha && payload.len() >= 5 && payload[0] == 0x2f,
                    "invalid VP8L"
                );
                let bits = u32::from_le_bytes(payload[1..5].try_into()?);
                ensure!(bits >> 29 == 0, "unsupported VP8L version");
                alpha = bits & (1 << 28) != 0;
                [(bits & 0x3fff) + 1, ((bits >> 14) & 0x3fff) + 1]
            }
            b"VP8 " => {
                ensure!(
                    payload.len() >= 10
                        && payload[0] & 1 == 0
                        && payload[3..6] == [0x9d, 0x01, 0x2a],
                    "invalid VP8 key frame"
                );
                [
                    (u16::from_le_bytes(payload[6..8].try_into()?) & 0x3fff) as u32,
                    (u16::from_le_bytes(payload[8..10].try_into()?) & 0x3fff) as u32,
                ]
            }
            b"VP8X" | b"ANIM" | b"ANMF" => anyhow::bail!("unexpected nested WebP container"),
            _ => continue,
        };
        ensure!(
            size.replace(value).is_none() && value.iter().all(|n| *n > 0),
            "invalid WebP image count/dimensions"
        );
    }
    Ok((size.context("missing WebP image bitstream")?, alpha))
}

pub fn inspect(bytes: &[u8]) -> Result<WebpInfo> {
    let chunks = container(bytes)?;
    let mut canvas = None;
    let mut flags = 0;
    let mut frames = Vec::new();
    let mut loop_count = None;
    let mut background = None;
    let mut still = Vec::new();
    for (index, chunk) in chunks.iter().enumerate() {
        let data = chunk.data;
        match chunk.kind {
            b"VP8X" => {
                ensure!(
                    index == 0 && data.len() == 10 && data[0] & 0xc1 == 0 && data[1..4] == [0; 3],
                    "invalid VP8X"
                );
                flags = data[0];
                canvas = Some([uint24(&data[4..])? + 1, uint24(&data[7..])? + 1]);
            }
            b"ANIM" => {
                ensure!(
                    flags & 2 != 0 && data.len() == 6 && loop_count.is_none() && still.is_empty(),
                    "invalid ANIM"
                );
                background = Some(u32::from_le_bytes(data[..4].try_into()?));
                loop_count = Some(u16::from_le_bytes(data[4..6].try_into()?));
            }
            b"ANMF" => {
                ensure!(
                    loop_count.is_some() && data.len() >= 16 && data[15] & !3 == 0,
                    "invalid ANMF"
                );
                let frame = FrameInfo {
                    x: uint24(data)? * 2,
                    y: uint24(&data[3..])? * 2,
                    width: uint24(&data[6..])? + 1,
                    height: uint24(&data[9..])? + 1,
                    duration: uint24(&data[12..])?,
                    flags: data[15],
                };
                let canvas = canvas.context("animation is missing VP8X")?;
                ensure!(
                    frame.x + frame.width <= canvas[0] && frame.y + frame.height <= canvas[1],
                    "frame exceeds canvas"
                );
                ensure!(
                    image_size(&self::chunks(&data[16..])?)?.0 == [frame.width, frame.height],
                    "ANMF bitstream dimensions mismatch"
                );
                frames.push(frame);
            }
            b"ALPH" | b"VP8 " | b"VP8L" => {
                ensure!(
                    loop_count.is_none(),
                    "animation contains a top-level still image"
                );
                still.push(Chunk {
                    kind: chunk.kind,
                    data,
                });
            }
            _ => (),
        }
    }
    if loop_count.is_none() {
        ensure!(flags & 2 == 0, "animation flag without ANIM");
        let (size, alpha) = image_size(&still)?;
        ensure!(
            canvas.is_none() || canvas == Some(size),
            "VP8X bitstream dimensions mismatch"
        );
        canvas = Some(size);
        if alpha {
            flags |= 0x10;
        }
    } else {
        ensure!(
            !frames.is_empty() && still.is_empty(),
            "animation has no frames"
        );
    }
    Ok(WebpInfo {
        canvas: canvas.context("WebP canvas missing")?,
        frames,
        loop_count,
        background,
        has_metadata: chunks
            .iter()
            .any(|c| matches!(c.kind, b"ICCP" | b"EXIF" | b"XMP ")),
        flags,
    })
}

fn chunk(output: &mut Vec<u8>, kind: &[u8], data: &[u8]) -> Result<()> {
    output.extend_from_slice(kind);
    output.extend_from_slice(&u32::try_from(data.len())?.to_le_bytes());
    output.extend_from_slice(data);
    if data.len() & 1 != 0 {
        output.push(0);
    }
    Ok(())
}

fn put24(output: &mut Vec<u8>, value: u32) -> Result<()> {
    ensure!(value <= MAX_DURATION, "WebP uint24 overflow");
    output.extend_from_slice(&value.to_le_bytes()[..3]);
    Ok(())
}

/// webpmux optimizes one frame to a still, losing ANIM/ANMF timing. For a
/// multi-logical-frame constant animation retain a real, timed ANMF instead.
/// Copy the encoded chunks verbatim; never decode/re-encode or duplicate pixels.
pub fn single_frame_animation(still: &[u8], canvas: [u32; 2], frame: FrameInfo) -> Result<Vec<u8>> {
    let info = inspect(still)?;
    ensure!(
        info.loop_count.is_none() && info.canvas == [frame.width, frame.height],
        "single run requires a matching still WebP"
    );
    ensure!(
        frame.flags == REPLACE_NO_DISPOSE
            && frame.duration > 0
            && frame.x % 2 == 0
            && frame.y % 2 == 0,
        "invalid single-run semantics"
    );
    ensure!(canvas.iter().all(|n| *n > 0), "empty WebP canvas");
    let chunks = container(still)?;
    let mut output = Vec::from(b"RIFF\0\0\0\0WEBP");
    let mut extended = vec![info.flags | 2, 0, 0, 0];
    put24(&mut extended, canvas[0] - 1)?;
    put24(&mut extended, canvas[1] - 1)?;
    chunk(&mut output, b"VP8X", &extended)?;
    for c in chunks.iter().filter(|c| c.kind == b"ICCP") {
        chunk(&mut output, c.kind, c.data)?;
    }
    chunk(&mut output, b"ANIM", &[0; 6])?; // transparent background, infinite loop
    let mut image = Vec::new();
    for n in [
        frame.x / 2,
        frame.y / 2,
        frame.width - 1,
        frame.height - 1,
        frame.duration,
    ] {
        put24(&mut image, n)?;
    }
    image.push(frame.flags);
    for c in chunks
        .iter()
        .filter(|c| matches!(c.kind, b"ALPH" | b"VP8 " | b"VP8L"))
    {
        chunk(&mut image, c.kind, c.data)?;
    }
    chunk(&mut output, b"ANMF", &image)?;
    for c in chunks
        .iter()
        .filter(|c| !matches!(c.kind, b"VP8X" | b"ICCP" | b"ALPH" | b"VP8 " | b"VP8L"))
    {
        chunk(&mut output, c.kind, c.data)?;
    }
    let size = u32::try_from(output.len() - 8)?;
    output[4..8].copy_from_slice(&size.to_le_bytes());
    let checked = inspect(&output)?;
    ensure!(
        checked.canvas == canvas && checked.frames == [frame],
        "single-run WebP validation failed"
    );
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    // Container/header fixtures only; integration tests use actual cwebp pixels.
    fn still() -> Vec<u8> {
        let mut b = Vec::from(b"RIFF\0\0\0\0WEBP");
        let bits = 7u32 | (5 << 14) | (1 << 28);
        let mut image = vec![0x2f];
        image.extend(bits.to_le_bytes());
        chunk(&mut b, b"VP8L", &image).unwrap();
        b[4..8].copy_from_slice(&((12 + 8 + 6 - 8) as u32).to_le_bytes());
        b
    }
    fn frame() -> FrameInfo {
        FrameInfo {
            x: 0,
            y: 0,
            width: 8,
            height: 6,
            duration: 5000,
            flags: 2,
        }
    }

    #[test]
    fn one_physical_frame_retains_timing_loop_alpha_and_encoded_chunks() {
        let source = still();
        let bytes = single_frame_animation(&source, [8, 6], frame()).unwrap();
        let info = inspect(&bytes).unwrap();
        assert_eq!(info.frames, [frame()]);
        assert_eq!(info.loop_count, Some(0));
        assert_eq!(info.background, Some(0));
        assert_eq!(info.flags, 0x12);
        let output = container(&bytes).unwrap();
        let anmf = output.iter().find(|c| c.kind == b"ANMF").unwrap();
        assert_eq!(&anmf.data[16..], &source[12..]);
        crate::finalize::validate_webp(&bytes, 1, [8, 6], &[5000]).unwrap();
    }

    #[test]
    fn validation_checks_durations_flags_dimensions_loops_and_truncation() {
        let original = single_frame_animation(&still(), [8, 6], frame()).unwrap();
        for offset in [20, 24, 27, 38, 42, 52, 58, 64, 67, 76] {
            // VP8X flags/size, background, loop, x, width, duration, ANMF
            // disposal flags, and the encoded VP8L signature respectively.
            let mut b = original.clone();
            b[offset] ^= 1;
            assert!(
                crate::finalize::validate_webp(&b, 1, [8, 6], &[5000]).is_err(),
                "offset {offset}"
            );
        }
        for end in 0..original.len() {
            assert!(inspect(&original[..end]).is_err());
        }
        let mut blending = original.clone();
        blending[67] = 0; // Valid ANMF flag value, but not replacement semantics.
        assert!(inspect(&blending).is_ok());
        assert!(crate::finalize::validate_webp(&blending, 1, [8, 6], &[5000]).is_err());
        assert!(crate::finalize::validate_webp(&original, 2, [8, 6], &[2500, 2500]).is_err());
        assert!(crate::finalize::validate_webp(&original, 1, [8, 6], &[]).is_err());
        let mut mismatch = frame();
        mismatch.width = 7;
        assert!(single_frame_animation(&still(), [8, 6], mismatch).is_err());
        assert!(single_frame_animation(&original, [8, 6], frame()).is_err());
    }
}
