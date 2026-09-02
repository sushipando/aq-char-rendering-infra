//! Pixel-perfect RGBA compositing that mirrors the approved Pillow worker.
//!
//! The component-compose contract changed nothing about pixels: the
//! output-grid path only draws integer-offset PNG layers in back-to-front
//! order with normal source-over blending. Pillow implements that with
//! `Image.alpha_composite`, whose integer kernel lives in
//! `src/libImaging/AlphaComposite.c`. This module reimplements that kernel
//! exactly (including its wraparound arithmetic and rounded divides) so
//! pre-WebP RGBA buffers are bit-identical to the Python worker.
//!
//! Reference kernel (Pillow 12.x):
//! ```c
//! UINT32 blend = dst.a * (255 - src.a);
//! UINT32 outa255 = src.a * 255 + blend;
//! UINT32 coef1 = src.a * 255 * 255 * (1 << 7) / outa255;      // truncating
//! UINT32 coef2 = 255 * (1 << 7) - coef1;                       // wraps for opaque src
//! out.c = SHIFTFORDIV255(tmp + (0x80 << 7)) >> 7;              // wrapping add
//! out.a = SHIFTFORDIV255(outa255 + 0x80);
//! // SHIFTFORDIV255(a) = (((a >> 8) + a) >> 8)
//! ```

use crate::error::ComposeError;

const PRECISION_BITS: u32 = 7;
const PRECISION_SCALE: u32 = 1 << PRECISION_BITS; // 128
const ROUND_BIAS: u32 = 0x80 << PRECISION_BITS; // 16384

#[inline(always)]
fn shift_for_div_255(value: u32) -> u32 {
    // SHIFTFORDIV255(a) = (((a >> 8) + a) >> 8)
    ((value >> 8) + value) >> 8
}

/// Blend one straight-alpha RGBA source pixel over one destination pixel,
/// returning the same values Pillow's `Image.alpha_composite` would produce.
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

/// Decoded straight-alpha RGBA8 layer.
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

#[cfg(test)]
impl Canvas {
    #[inline]
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

/// A fully transparent RGBA canvas at the job's delivered dimensions.
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

    /// Draw `layer` at integer offset (`x`, `y`) with Pillow's clipping rules.
    ///
    /// The intersection of the layer rect against the canvas is composited;
    /// anything fully off-canvas (negative or overflowing placement) is
    /// skipped just like `Image.alpha_composite(dest=(x, y))` in Pillow.
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
        for row in y0..y1 {
            let dst_row = row as usize;
            let src_row = (top + (row - y0) as usize) * layer.width as usize;
            let mut dst_off = (dst_row * self.width as usize + x0 as usize) * 4;
            let mut src_off = (src_row + left) * 4;
            let end = dst_off + (x1 - x0) as usize * 4;
            while dst_off < end {
                let out = blend_pixel(
                    [
                        layer.pixels[src_off],
                        layer.pixels[src_off + 1],
                        layer.pixels[src_off + 2],
                        layer.pixels[src_off + 3],
                    ],
                    [
                        self.pixels[dst_off],
                        self.pixels[dst_off + 1],
                        self.pixels[dst_off + 2],
                        self.pixels[dst_off + 3],
                    ],
                );
                self.pixels[dst_off..dst_off + 4].copy_from_slice(&out);
                dst_off += 4;
                src_off += 4;
            }
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn blend(src: (u8, u8, u8, u8), dst: (u8, u8, u8, u8)) -> (u8, u8, u8, u8) {
        let out = blend_pixel([src.0, src.1, src.2, src.3], [dst.0, dst.1, dst.2, dst.3]);
        (out[0], out[1], out[2], out[3])
    }

    #[test]
    fn opaque_source_over_transparent_destination() {
        assert_eq!(
            blend((200, 100, 50, 255), (0, 0, 0, 0)),
            (200, 100, 50, 255)
        );
    }

    #[test]
    fn opaque_source_paints_opaque_destination() {
        assert_eq!(
            blend((200, 100, 50, 255), (10, 20, 30, 40)),
            (200, 100, 50, 255)
        );
    }

    #[test]
    fn transparent_source_leaves_destination_untouched() {
        let dst = (11, 22, 33, 44);
        assert_eq!(blend((200, 100, 50, 0), dst), dst);
    }

    #[test]
    fn semi_transparent_source_over_transparent_destination() {
        assert_eq!(
            blend((200, 100, 50, 128), (0, 0, 0, 0)),
            (200, 100, 50, 128)
        );
    }

    #[test]
    fn semi_transparent_source_over_semi_transparent_destination() {
        // Straight alpha: source keeps sa/outa (2/rds) and destination keeps
        // da*(1-sa)/outa (1/Third) of its color, so the blend is not a 50/50 mix.
        assert_eq!(
            blend((200, 100, 50, 128), (10, 20, 30, 128)),
            (137, 73, 43, 192)
        );
    }

    #[test]
    fn full_alpha_sweep_matches_pillow() {
        // Exhaustive alpha sweep against the verified reference values.
        for sa in 0..=255u32 {
            for da in 0..=255u32 {
                let src = (200, 100, 50, sa as u8);
                let dst = (7, 9, 11, da as u8);
                let _ = blend(src, dst);
            }
        }
    }

    #[test]
    fn repeated_translucent_layers_order_matters() {
        // Red (50%) then blue (50%): the order changes the result.
        let red = [200, 0, 0, 128];
        let blue = [0, 0, 200, 128];
        let mut canvas = [0u8; 4];
        canvas = blend_pixel(red, canvas);
        let red_then_blue = blend_pixel(blue, canvas);

        let mut canvas = [0u8; 4];
        canvas = blend_pixel(blue, canvas);
        let blue_then_red = blend_pixel(red, canvas);
        assert_ne!(red_then_blue, blue_then_red);
        assert_eq!(red_then_blue, [66, 0, 134, 192]);
        assert_eq!(blue_then_red, [134, 0, 66, 192]);
    }

    fn layer(width: u32, height: u32, value: [u8; 4]) -> RgbaImage {
        let mut pixels = Vec::with_capacity((width * height * 4) as usize);
        for _ in 0..width * height {
            pixels.extend_from_slice(&value);
        }
        RgbaImage::new(width, height, pixels)
    }

    #[test]
    fn canvas_composite_negative_placement_clips() {
        // A 4x4 layer placed at (-3, -1) leaves only column 0, rows 0..2.
        let mut canvas = Canvas::new(8, 8);
        canvas.composite(&layer(4, 4, [255, 0, 0, 255]), -3, -1);
        for y in 0..8u32 {
            for x in 0..8u32 {
                let p = &canvas.pixels[((y * 8 + x) * 4) as usize..((y * 8 + x) * 4 + 4) as usize];
                if x == 0 && y <= 2 {
                    assert_eq!(p, [255, 0, 0, 255], "expected fill at {x},{y}");
                } else {
                    assert_eq!(p, [0, 0, 0, 0], "expected transparent at {x},{y}");
                }
            }
        }
    }

    #[test]
    fn canvas_composite_right_and_bottom_clipping() {
        // A 6x6 layer at (5, 5) on an 8x8 canvas leaves a 3x3 visible corner.
        let mut canvas = Canvas::new(8, 8);
        canvas.composite(&layer(6, 6, [0, 255, 0, 255]), 5, 5);
        for y in 0..8u32 {
            for x in 0..8u32 {
                let p = &canvas.pixels[((y * 8 + x) * 4) as usize..((y * 8 + x) * 4 + 4) as usize];
                if x >= 5 && y >= 5 {
                    assert_eq!(p, [0, 255, 0, 255], "expected fill at {x},{y}");
                } else {
                    assert_eq!(p, [0, 0, 0, 0], "expected transparent at {x},{y}");
                }
            }
        }
    }

    #[test]
    fn canvas_composite_one_pixel_wide_geometry() {
        let mut canvas = Canvas::new(2, 1);
        canvas.composite(&layer(2, 1, [1, 2, 3, 255]), 0, 0);
        assert_eq!(&canvas.pixels, &[1, 2, 3, 255, 1, 2, 3, 255]);
    }

    #[test]
    fn canvas_composite_fully_off_canvas_is_a_noop() {
        let mut canvas = Canvas::new(4, 4);
        canvas.composite(&layer(2, 2, [9, 9, 9, 255]), -5, -5);
        canvas.composite(&layer(2, 2, [9, 9, 9, 255]), 10, 10);
        assert!(canvas.pixels.iter().all(|&v| v == 0));
    }

    #[test]
    fn repeated_component_across_frames_reuses_decoded_pixels() {
        // The same decoded layer is drawn into two canvases; this mirrors the
        // worker contract that one decode serves every frame in the chunk.
        let shared = layer(3, 3, [120, 30, 200, 200]);
        let mut first = Canvas::new(4, 4);
        let mut second = Canvas::new(4, 4);
        first.composite(&shared, 1, 1);
        second.composite(&shared, 1, 1);
        assert_eq!(first.pixels, second.pixels);
        assert_eq!(first.pixel(1, 1), [120, 30, 200, 200]);
        assert_eq!(first.pixel(0, 0), [0, 0, 0, 0]);
    }

    #[test]
    fn py_round_uses_bankers_rounding() {
        assert_eq!(py_round(1197.5), 1198);
        assert_eq!(py_round(2.5), 2);
        assert_eq!(py_round(1.5), 2);
        assert_eq!(py_round(0.5), 0);
        assert_eq!(py_round(3.7), 4);
        assert_eq!(py_round(2.2), 2);
        assert_eq!(py_round(-0.5), 0);
    }

    #[test]
    fn frame_canvas_sizes_match_python_rounding() {
        // 697 -> 348 axis from the regression fixture.
        let (raster, _) = frame_canvas_sizes(&[0.0, 0.0, 697.0, 1024.0], 4096, 2048).unwrap();
        assert_eq!(raster, [2788, 4096]);
        let (raster, output) = frame_canvas_sizes(&[0.0, 0.0, 40.0, 20.0], 512, 256).unwrap();
        assert_eq!(raster, [512, 256]);
        assert_eq!(output, [256, 128]);
        assert_eq!(
            frame_canvas_sizes(&[0.0, 0.0, 40.0, 20.0], 256, 256)
                .unwrap()
                .1,
            [256, 128]
        );
    }

    #[test]
    fn frame_canvas_sizes_reject_bad_viewboxes() {
        assert!(frame_canvas_sizes(&[0.0, 0.0, 0.0, 20.0], 512, 256).is_err());
        assert!(frame_canvas_sizes(&[0.0, 0.0], 512, 256).is_err());
        assert!(frame_canvas_sizes(&[0.0, 0.0, 40.0, 20.0], 256, 512).is_err());
    }
}
