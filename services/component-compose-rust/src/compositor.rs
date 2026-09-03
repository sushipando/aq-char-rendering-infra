//! RGBA layer compositing for the component-compose worker.
//!
//! The compositor works in **premultiplied RGBA8** and blends layers with
//! Porter-Duff source-over using AArch64 NEON SIMD (via the `wide` crate)
//! on the Graviton Lambda target:
//!
//! ```text
//! decoded straight RGBA8 layer
//!         ↓  premultiply once per layer (SIMD)
//! premultiplied RGBA8 layer
//!         ↓  source-over per layer (SIMD)
//! premultiplied RGBA8 frame
//!         ↓  unpremultiply once per frame (SIMD)
//! straight RGBA8 frame  →  PNG / WebP
//! ```
//!
//! The scalar reference in [`scalar`] is the permanent correctness oracle;
//! [`wide`] must match it byte-for-byte (enforced by [`tests`]). The
//! `blend_pixel` function below is the legacy Pillow-exact kernel kept for
//! reference only; the production path never calls it.

mod scalar;
#[cfg(test)]
mod tests;
mod wide;

pub use scalar::{premultiply_rgba_scalar, source_over_scalar, unpremultiply_rgba_scalar};
pub use wide::{premultiply_rgba, source_over, unpremultiply_rgba};

use crate::error::ComposeError;

const PRECISION_BITS: u32 = 7;
const PRECISION_SCALE: u32 = 1 << PRECISION_BITS; // 128
const ROUND_BIAS: u32 = 0x80 << PRECISION_BITS; // 16384

#[inline(always)]
fn shift_for_div_255(value: u32) -> u32 {
    // SHIFTFORDIV255(a) = (((a >> 8) + a) >> 8)
    ((value >> 8) + value) >> 8
}

/// Legacy Pillow-exact straight-alpha blend, kept as a reference.
///
/// The production compositor uses the premultiplied SIMD path
/// ([`source_over`]); this function documents the historical
/// `Image.alpha_composite` kernel (including its wraparound arithmetic and
/// rounded divides) that the previous worker used.
#[inline(always)]
pub fn blend_pixel(src: [u8; 4], dst: [u8; 4]) -> [u8; 4] {
    let sa = src[3] as u32;
    if sa == 0 {
        // Pillow copies the destination unchanged for fully transparent sources.
        return dst;
    }
    let (sr, sg, sb) = (src[0] as u32, src[1] as u32, src[2] as u32);
    let (dr, dg, db) = (dst[0] as u32, dst[1] as u32, dst[2] as u32);
    let da = dst[3] as u32;

    let blend = da * (255 - sa);
    let outa255 = sa * 255 + blend;
    let coef1 = (sa * 255 * 255 * PRECISION_SCALE) / outa255;
    // Pillow computes this in UINT32; for a fully opaque source coef1 exceeds
    // 255*128 and coef2 wraps negative. The higher-latency reconstitution
    // still lands on the exact source color, so replicate the wraparound.
    let coef2 = (255 * PRECISION_SCALE).wrapping_sub(coef1);

    let r = shift_for_div_255(
        (sr.wrapping_mul(coef1))
            .wrapping_add(dr.wrapping_mul(coef2))
            .wrapping_add(ROUND_BIAS),
    ) >> PRECISION_BITS;
    let g = shift_for_div_255(
        (sg.wrapping_mul(coef1))
            .wrapping_add(dg.wrapping_mul(coef2))
            .wrapping_add(ROUND_BIAS),
    ) >> PRECISION_BITS;
    let b = shift_for_div_255(
        (sb.wrapping_mul(coef1))
            .wrapping_add(db.wrapping_mul(coef2))
            .wrapping_add(ROUND_BIAS),
    ) >> PRECISION_BITS;
    let a = shift_for_div_255(outa255 + 0x80);
    [r as u8, g as u8, b as u8, a as u8]
}

/// Decoded RGBA8 layer.
///
/// `png::decode_rgba8` returns straight-alpha pixels; the worker calls
/// [`premultiply_rgba`] once after decode and reuses the premultiplied
/// pixels across every frame in the chunk.
#[derive(Clone, Debug)]
pub struct RgbaImage {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

impl RgbaImage {
    pub fn new(width: u32, height: u32, pixels: Vec<u8>) -> Self {
        debug_assert_eq!(pixels.len(), width as usize * height as usize * 4);
        RgbaImage {
            width,
            height,
            pixels,
        }
    }
}

/// A fully transparent RGBA canvas at the job's delivered dimensions.
///
/// During compositing the canvas holds **premultiplied** RGBA8; the worker
/// unpremultiplies once before encoding.
pub struct Canvas {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

impl Canvas {
    pub fn new(width: u32, height: u32) -> Self {
        Canvas {
            width,
            height,
            pixels: vec![0u8; width as usize * height as usize * 4],
        }
    }

    pub fn from_pixels(width: u32, height: u32, pixels: Vec<u8>) -> Self {
        debug_assert_eq!(pixels.len(), width as usize * height as usize * 4);
        Canvas {
            width,
            height,
            pixels,
        }
    }

    /// Draw a **premultiplied** `layer` at integer offset (`x`, `y`) with
    /// Pillow's clipping rules.
    ///
    /// The intersection of the layer rect against the canvas is composited
    /// with SIMD source-over; anything fully off-canvas (negative or
    /// overflowing placement) is skipped just like
    /// `Image.alpha_composite(dest=(x, y))` in Pillow.
    pub fn composite(&mut self, layer: &RgbaImage, x: i64, y: i64) {
        let iw = layer.width as i64;
        let ih = layer.height as i64;
        let canvas_w = self.width as i64;
        let canvas_h = self.height as i64;
        let x0 = x.max(0);
        let y0 = y.max(0);
        let x1 = (x + iw).min(canvas_w);
        let y1 = (y + ih).min(canvas_h);
        if x0 >= x1 || y0 >= y1 || x0 >= canvas_w || y0 >= canvas_h {
            return;
        }
        let left = (x0 - x) as usize; // source column offset, >= 0 by clipping
        let top = (y0 - y) as usize;
        let row_pixels = (x1 - x0) as usize;
        for row in y0..y1 {
            let dst_row = row as usize;
            let src_row = (top + (row - y0) as usize) * layer.width as usize;
            let dst_start = (dst_row * self.width as usize + x0 as usize) * 4;
            let src_start = (src_row + left) * 4;
            let row_bytes = row_pixels * 4;
            source_over(
                &mut self.pixels[dst_start..dst_start + row_bytes],
                &layer.pixels[src_start..src_start + row_bytes],
            );
        }
    }

    #[cfg(test)]
    pub fn pixel(&self, x: u32, y: u32) -> [u8; 4] {
        let offset = ((y * self.width + x) * 4) as usize;
        [
            self.pixels[offset],
            self.pixels[offset + 1],
            self.pixels[offset + 2],
            self.pixels[offset + 3],
        ]
    }
}

/// Python's `round()` uses banker's rounding (ties to even); `f64::round`
/// rounds half away from zero. Frame canvas math depends on the Python form,
/// so replicate it here.
pub fn py_round(value: f64) -> i64 {
    let floor = value.floor();
    let fraction = value - floor;
    if fraction < 0.5 {
        floor as i64
    } else if fraction > 0.5 {
        floor as i64 + 1
    } else {
        let integer = floor as i64;
        if integer % 2 == 0 {
            integer
        } else {
            integer + 1
        }
    }
}

/// Raster and delivered canvas sizes using the legacy Python rounding.
///
/// Mirrors `_frame_canvas_sizes` in
/// `aqw_char_renderer/stages/compose_frames.py`: the delivered dimensions
/// derive from the *rounded* raster canvas, which matters for aspect-ratio
/// dimensions that become odd at the raster size (e.g. 697 -> 348).
pub fn frame_canvas_sizes(
    viewbox: &[f64],
    raster_size: i64,
    output_size: i64,
) -> Result<([i64; 2], [i64; 2]), ComposeError> {
    if viewbox.len() != 4 || viewbox[2] <= 0.0 || viewbox[3] <= 0.0 {
        return Err(ComposeError::invalid(
            "Prepare manifest has no usable shared viewbox",
        ));
    }
    if raster_size <= 0 || output_size <= 0 || output_size > raster_size {
        return Err(ComposeError::invalid(
            "Output size must be positive and cannot exceed raster size",
        ));
    }
    let pixel_scale = raster_size as f64 / viewbox[2].max(viewbox[3]);
    let raster_canvas = [
        py_round(viewbox[2] * pixel_scale).max(1),
        py_round(viewbox[3] * pixel_scale).max(1),
    ];
    if output_size == raster_size {
        return Ok((raster_canvas, raster_canvas));
    }
    let scale = output_size as f64 / raster_canvas[0].max(raster_canvas[1]) as f64;
    let output_canvas = [
        py_round(raster_canvas[0] as f64 * scale).max(1),
        py_round(raster_canvas[1] as f64 * scale).max(1),
    ];
    Ok((raster_canvas, output_canvas))
}
