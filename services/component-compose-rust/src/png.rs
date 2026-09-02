//! PNG decode/encode for component rasters and the pre-cwebp frame file.

use crate::compositor::RgbaImage;
use crate::error::ComposeError;

/// Decode any standard PNG (including palette, gray, and 16-bit inputs from
/// older saves) into straight-alpha RGBA8, matching Pillow's
/// `convert("RGBA")` normalization.
pub fn decode_rgba8(bytes: &[u8]) -> Result<RgbaImage, ComposeError> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    decoder.set_transformations(
        png::Transformations::EXPAND | png::Transformations::ALPHA | png::Transformations::STRIP_16,
    );
    let mut reader = decoder
        .read_info()
        .map_err(|error| ComposeError::Png(error.to_string()))?;
    let mut buffer = vec![0u8; reader.output_buffer_size().unwrap_or(0)];
    let info = reader
        .next_frame(&mut buffer)
        .map_err(|error| ComposeError::Png(error.to_string()))?;
    let width = info.width;
    let height = info.height;
    let written = info.buffer_size();
    buffer.truncate(written);
    // The png 0.18 decoder only expands to 8-bit; gray and gray-alpha inputs
    // arrive as-is, so normalize every color type to straight RGBA8 exactly
    // like Pillow's `convert("RGBA")`.
    let output_color = reader.output_color_type().0;
    let pixels = match output_color {
        png::ColorType::Rgba => buffer,
        png::ColorType::Rgb => {
            let mut out = Vec::with_capacity(width as usize * height as usize * 4);
            for pixel in buffer.as_chunks::<3>().0 {
                out.extend_from_slice(&[pixel[0], pixel[1], pixel[2], 255]);
            }
            out
        }
        png::ColorType::GrayscaleAlpha => {
            let mut out = Vec::with_capacity(width as usize * height as usize * 4);
            for pixel in buffer.as_chunks::<2>().0 {
                out.extend_from_slice(&[pixel[0], pixel[0], pixel[0], pixel[1]]);
            }
            out
        }
        png::ColorType::Grayscale => {
            let mut out = Vec::with_capacity(width as usize * height as usize * 4);
            for &gray in &buffer {
                out.extend_from_slice(&[gray, gray, gray, 255]);
            }
            out
        }
        other => {
            return Err(ComposeError::Png(format!(
                "unsupported normalized color type {other:?} after transformations"
            )));
        }
    };
    if pixels.len() != width as usize * height as usize * 4 {
        return Err(ComposeError::Png(
            "decoded pixel count mismatch".to_string(),
        ));
    }
    Ok(RgbaImage::new(width, height, pixels))
}

/// Encode straight-alpha RGBA8 as an 8-bit PNG (the intermediate file fed to
/// the pinned `cwebp` and the lossless local debug frames).
pub fn encode_rgba8(width: u32, height: u32, pixels: &[u8]) -> Result<Vec<u8>, ComposeError> {
    let mut output = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut output, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder
            .write_header()
            .map_err(|error| ComposeError::Png(error.to_string()))?;
        writer
            .write_image_data(pixels)
            .map_err(|error| ComposeError::Png(error.to_string()))?;
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_rgba() {
        let pixels: Vec<u8> = (0..16 * 16 * 4)
            .map(|index| (index * 7 % 256) as u8)
            .collect();
        let encoded = encode_rgba8(16, 16, &pixels).unwrap();
        let decoded = decode_rgba8(&encoded).unwrap();
        assert_eq!(decoded.width, 16);
        assert_eq!(decoded.height, 16);
        assert_eq!(decoded.pixels, pixels);
    }

    #[test]
    fn decodes_opaque_rgb_png_as_rgba() {
        // A tiny 2x1 RGB PNG (left red, right green) saved by Pillow.
        let bytes = [
            137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 2, 0, 0, 0, 1,
            8, 2, 0, 0, 0, 123, 64, 232, 221, 0, 0, 0, 15, 73, 68, 65, 84, 120, 156, 99, 248, 207,
            192, 192, 240, 159, 1, 0, 7, 255, 1, 255, 1, 127, 137, 167, 0, 0, 0, 0, 73, 69, 78, 68,
            174, 66, 96, 130,
        ];
        let decoded = decode_rgba8(&bytes).unwrap();
        assert_eq!(decoded.width, 2);
        assert_eq!(decoded.height, 1);
        assert_eq!(decoded.pixels, vec![255, 0, 0, 255, 0, 255, 0, 255]);
    }

    #[test]
    fn rejects_garbage() {
        assert!(decode_rgba8(b"not a png").is_err());
    }
}
