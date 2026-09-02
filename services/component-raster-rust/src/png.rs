//! PNG decode (for tests) — encoding lives in `raster.rs`.

use crate::compositor::RgbaImage;
use crate::error::RasterError;

pub fn decode_rgba8(bytes: &[u8]) -> Result<RgbaImage, RasterError> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    decoder.set_transformations(
        png::Transformations::EXPAND | png::Transformations::ALPHA | png::Transformations::STRIP_16,
    );
    let mut reader = decoder
        .read_info()
        .map_err(|error| RasterError::Png(error.to_string()))?;
    let mut buffer = vec![0u8; reader.output_buffer_size().unwrap_or(0)];
    let info = reader
        .next_frame(&mut buffer)
        .map_err(|error| RasterError::Png(error.to_string()))?;
    let width = info.width;
    let height = info.height;
    buffer.truncate(info.buffer_size());
    let color = reader.output_color_type().0;
    let pixels = match color {
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
            return Err(RasterError::Png(format!(
                "unsupported normalized color type {other:?}"
            )));
        }
    };
    Ok(RgbaImage::new(width, height, pixels))
}
