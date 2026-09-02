//! Pillow-exact Lanczos resampling (port of `src/libImaging/Resample.c` with
//! the `RGBa` premultiply/unpremultiply from `Convert.c`).
//!
//! The component raster downsample must reproduce the Python worker's output
//! bit-for-bit, including the alpha-bbox crop afterwards. fast_image_resize's
//! integer path can differ by 1/255 at AA edges — enough to flip an alpha-row
//! and change recorded dimensions — so this module mirrors Pillow's float
//! kernel, 2^22 fixed-point coefficients, 2^21 rounding bias, and the
//! horizontal-then-vertical two-pass layout exactly.

use crate::compositor::RgbaImage;
use crate::error::RasterError;

const PRECISION_BITS: i32 = 22;
const PRECISION_SCALE: f64 = (1i64 << PRECISION_BITS) as f64; // 2^22
const ROUNDING_BIAS: i32 = 1 << (PRECISION_BITS - 1); // 2^21

fn sinc(x: f64) -> f64 {
    if x == 0.0 {
        1.0
    } else {
        let t = x * std::f64::consts::PI;
        t.sin() / t
    }
}

fn lanczos(x: f64) -> f64 {
    // truncated sinc
    if (-3.0..3.0).contains(&x) {
        sinc(x) * sinc(x / 3.0)
    } else {
        0.0
    }
}

/// Port of `precompute_coeffs` for one axis: returns per-output-pixel
/// `(xmin, count, k)` with float weights.
struct Coeffs {
    xmin: Vec<usize>,
    xmax: Vec<usize>,
    /// Fixed-point (2^22) coefficients, ksize per output pixel.
    kk: Vec<i32>,
    ksize: usize,
}

fn precompute_coeffs(in_size: usize, in0: f64, in1: f64, out_size: usize) -> Coeffs {
    let scale = (in1 - in0) / out_size as f64;
    let filterscale = if scale < 1.0 { 1.0 } else { scale };
    let support = 3.0 * filterscale;
    let ksize = (support.ceil() as usize) * 2 + 1;
    let inv_filterscale = 1.0 / filterscale;

    let mut xmin = Vec::with_capacity(out_size);
    let mut xmax = Vec::with_capacity(out_size);
    let mut kk = vec![0i32; out_size * ksize];
    for xx in 0..out_size {
        let center = in0 + (xx as f64 + 0.5) * scale;
        let mut lo = (center - support + 0.5) as i64;
        if lo < 0 {
            lo = 0;
        }
        let mut hi = (center + support + 0.5) as i64;
        if hi > in_size as i64 {
            hi = in_size as i64;
        }
        let count = (hi - lo) as usize;
        let mut sum = 0.0f64;
        let mut weights = vec![0.0f64; count];
        for (index, weight) in weights.iter_mut().enumerate() {
            let w = lanczos((index as f64 + lo as f64 - center + 0.5) * inv_filterscale);
            *weight = w;
            sum += w;
        }
        if sum != 0.0 {
            for weight in &mut weights {
                *weight /= sum;
            }
        }
        let offset = xx * ksize;
        for index in 0..count {
            // normalize_coeffs_8bpc: round half away from zero to 2^22 scale.
            kk[offset + index] = (weights[index] * PRECISION_SCALE).round() as i32;
        }
        if count < ksize {
            for index in count..ksize {
                kk[offset + index] = 0;
            }
        }
        // Track count via xmax entries (used by callers as bounds).
        xmin.push(lo as usize);
        xmax.push(count.max(1));
        let _ = hi;
    }
    Coeffs {
        xmin,
        xmax,
        kk,
        ksize,
    }
}

#[inline]
fn clip8(value: i32) -> u8 {
    // clip8_lookups semantics: (value >> 22) clamped to 0..255.
    let shifted = value >> PRECISION_BITS;
    if shifted <= 0 {
        0
    } else if shifted >= 255 {
        255
    } else {
        shifted as u8
    }
}

/// `MULDIV255(a, b) = SHIFTFORDIV255(a*b + 128)` — Pillow's premultiply step.
#[inline]
fn muldiv255(a: u32, b: u32) -> u8 {
    let value = a * b + 128;
    (((value >> 8) + value) >> 8) as u8
}

/// Pillow `rgba2rgbA`: premultiplied -> straight (used after resampling).
#[inline]
fn unpremultiply_channel(premult: u8, alpha: u8) -> u8 {
    if alpha == 255 || alpha == 0 {
        premult
    } else {
        // CLIP8((255 * c) / alpha) — truncating integer division.
        let value = (255 * premult as u32) / alpha as u32;
        if value > 255 {
            255
        } else {
            value as u8
        }
    }
}

/// Two-pass Pillow-exact resize on straight RGBA with a fractional source box.
#[allow(clippy::too_many_arguments)]
pub fn resize_lanczos(
    source: &RgbaImage,
    out_w: usize,
    out_h: usize,
    box_left: f64,
    box_top: f64,
    box_right: f64,
    box_bottom: f64,
) -> Result<RgbaImage, RasterError> {
    let _in_w = source.width as usize;

    // Premultiply the whole patch exactly like `convert("RGBa")`.
    let mut premultiplied = Vec::with_capacity(source.pixels.len());
    for pixel in source.pixels.as_chunks::<4>().0 {
        let alpha = pixel[3] as u32;
        premultiplied.extend_from_slice(&[
            muldiv255(pixel[0] as u32, alpha),
            muldiv255(pixel[1] as u32, alpha),
            muldiv255(pixel[2] as u32, alpha),
            pixel[3],
        ]);
    }

    let in_w = source.width as usize;
    let _ = in_w;
    let in_h = source.height as usize;
    let coeffs_vert = precompute_coeffs(in_h, box_top, box_bottom, out_h);
    let use_horizontal = out_w != in_w || box_left != 0.0 || box_right != in_w as f64;
    let use_vertical = out_h != in_h || box_top != 0.0 || box_bottom != in_h as f64;

    if !use_horizontal && !use_vertical {
        return Ok(source.clone());
    }

    let ybox_first = coeffs_vert.xmin[0];
    let ybox_last = coeffs_vert.xmin[out_h - 1] + coeffs_vert.xmax[out_h - 1];

    // ---- horizontal pass ----------------------------------------------------
    let (temp, temp_h) = if use_horizontal {
        let coeffs_horiz = precompute_coeffs(in_w, box_left, box_right, out_w);
        let temp_h = ybox_last - ybox_first;
        let mut temp = vec![0u8; out_w * temp_h * 4];
        for yy in 0..temp_h {
            let src_row = (yy + ybox_first) * in_w * 4;
            for xx in 0..out_w {
                let lo = coeffs_horiz.xmin[xx];
                let count = coeffs_horiz.xmax[xx];
                let offset = xx * coeffs_horiz.ksize;
                let mut ss = [ROUNDING_BIAS; 4];
                for (index, coefficient) in
                    coeffs_horiz.kk[offset..offset + count].iter().enumerate()
                {
                    let pixel = src_row + (index + lo) * 4;
                    ss[0] += premultiplied[pixel] as i32 * coefficient;
                    ss[1] += premultiplied[pixel + 1] as i32 * coefficient;
                    ss[2] += premultiplied[pixel + 2] as i32 * coefficient;
                    ss[3] += premultiplied[pixel + 3] as i32 * coefficient;
                }
                let out_pixel = (yy * out_w + xx) * 4;
                temp[out_pixel] = clip8(ss[0]);
                temp[out_pixel + 1] = clip8(ss[1]);
                temp[out_pixel + 2] = clip8(ss[2]);
                temp[out_pixel + 3] = clip8(ss[3]);
            }
        }
        (temp, temp_h)
    } else {
        (premultiplied, source.height as usize)
    };

    // ---- vertical pass ------------------------------------------------------
    if !use_vertical {
        // Straighten the premultiplied temp and return.
        let mut output = Vec::with_capacity(temp.len());
        for pixel in temp.as_chunks::<4>().0 {
            output.extend_from_slice(&[
                unpremultiply_channel(pixel[0], pixel[3]),
                unpremultiply_channel(pixel[1], pixel[3]),
                unpremultiply_channel(pixel[2], pixel[3]),
                pixel[3],
            ]);
        }
        return Ok(RgbaImage::new(out_w as u32, temp_h as u32, output));
    }

    // Pillow shifts horizontal-pass bounds only when a horizontal pass ran;
    // otherwise the vertical pass reads the source directly.
    let ymin: Vec<usize> = if use_horizontal {
        coeffs_vert
            .xmin
            .iter()
            .map(|min| min.saturating_sub(ybox_first))
            .collect()
    } else {
        coeffs_vert.xmin.clone()
    };
    let mut output = vec![0u8; out_w * out_h * 4];
    for (yy, &lo) in ymin.iter().enumerate() {
        let count = coeffs_vert.xmax[yy];
        let offset = yy * coeffs_vert.ksize;
        for xx in 0..out_w {
            let mut ss = [ROUNDING_BIAS; 4];
            for (index, coefficient) in coeffs_vert.kk[offset..offset + count].iter().enumerate() {
                let pixel = ((index + lo) * out_w + xx) * 4;
                ss[0] += temp[pixel] as i32 * coefficient;
                ss[1] += temp[pixel + 1] as i32 * coefficient;
                ss[2] += temp[pixel + 2] as i32 * coefficient;
                ss[3] += temp[pixel + 3] as i32 * coefficient;
            }
            let out_pixel = (yy * out_w + xx) * 4;
            output[out_pixel] = clip8(ss[0]);
            output[out_pixel + 1] = clip8(ss[1]);
            output[out_pixel + 2] = clip8(ss[2]);
            output[out_pixel + 3] = clip8(ss[3]);
        }
    }

    // Unpremultiply exactly like `convert("RGBA")`.
    for pixel in output.as_chunks_mut::<4>().0 {
        let alpha = pixel[3];
        pixel[0] = unpremultiply_channel(pixel[0], alpha);
        pixel[1] = unpremultiply_channel(pixel[1], alpha);
        pixel[2] = unpremultiply_channel(pixel[2], alpha);
    }
    Ok(RgbaImage::new(out_w as u32, out_h as u32, output))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lanczos_kernel_matches_pillow_values() {
        // sinc(0)=1 by definition; lanczos(0)=1.
        assert!((lanczos(0.0) - 1.0).abs() < 1e-12);
        // lanczos(1) = sinc(1)*sinc(1/3) ~ 0.6366 * 0.8270...
        let expected = (std::f64::consts::PI).sin() / (std::f64::consts::PI)
            * ((std::f64::consts::PI / 3.0).sin() / (std::f64::consts::PI / 3.0));
        assert!((lanczos(1.0) - expected).abs() < 1e-12);
        assert_eq!(lanczos(3.0), 0.0);
        assert_eq!(lanczos(-0.5), lanczos(0.5));
    }

    #[test]
    fn identity_box_resize_copies() {
        let image = RgbaImage::new(4, 3, vec![255u8; 4 * 3 * 4]);
        let out = resize_lanczos(&image, 4, 3, 0.0, 0.0, 4.0, 3.0).unwrap();
        assert_eq!(out.pixels, image.pixels);
    }

    #[test]
    fn downscales_and_unpremultiplies() {
        // 4x4 fully-opaque grid downsampled to 2x2.
        let mut pixels = Vec::new();
        for _y in 0..4 {
            for x in 0..4 {
                pixels.extend_from_slice(&[40u8 + x * 10, 100, 200, 255]);
            }
        }
        let image = RgbaImage::new(4, 4, pixels);
        let out = resize_lanczos(&image, 2, 2, 0.0, 0.0, 4.0, 4.0).unwrap();
        assert_eq!((out.width, out.height), (2, 2));
        // Opaque output keeps straight colors near the source midpoint.
        for pixel in out.pixels.as_chunks::<4>().0 {
            assert_eq!(pixel[3], 255);
            assert!(pixel[0] >= 40 && pixel[0] <= 90);
        }
    }

    #[test]
    fn semi_transparent_unpremultiplies_correctly() {
        // Pillow's identity resize short-circuits to copy() before the RGBa
        // round trip, so mirror that: straight value passes through untouched.
        let straight = [200u8, 100, 50, 128];
        let image = RgbaImage::new(1, 1, straight.to_vec());
        let out = resize_lanczos(&image, 1, 1, 0.0, 0.0, 1.0, 1.0).unwrap();
        assert_eq!(out.pixels[0], straight[0]);
        assert_eq!(out.pixels[3], 128);

        // A real 2x1 -> 1x1 pass premultiplies then unpremultiplies with
        // Pillow's truncating (255 * c) / alpha.
        let wide = RgbaImage::new(2, 1, vec![200u8, 100, 50, 128, 200, 100, 50, 128]);
        let out = resize_lanczos(&wide, 1, 1, 0.0, 0.0, 2.0, 1.0).unwrap();
        let premult = muldiv255(200, 128);
        let expected = (255 * premult as u32) / 128;
        assert_eq!(out.pixels[0], expected as u8);
        assert_eq!(out.pixels[3], 128);
    }
}
