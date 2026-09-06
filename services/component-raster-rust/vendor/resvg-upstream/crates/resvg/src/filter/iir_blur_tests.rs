// Copyright 2026 the Resvg Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

use super::{BlurData, apply, gaussianiir2d, gen_coefficients};
use crate::filter::ImageRefMut;

// Keep the original column-at-a-time implementation as an independent
// regression oracle for the traversal change, including f64 rounding.
fn original_gaussianiir2d(d: &BlurData, buf: &mut [f64]) {
    let (lambda_x, dnu_x) = if d.sigma_x > 0.0 {
        let (lambda, dnu) = gen_coefficients(d.sigma_x, d.steps);
        for y in 0..d.height {
            for _ in 0..d.steps {
                let idx = d.width * y;
                for x in 1..d.width {
                    buf[idx + x] += dnu * buf[idx + x - 1];
                }
                let mut x = d.width - 1;
                while x > 0 {
                    buf[idx + x - 1] += dnu * buf[idx + x];
                    x -= 1;
                }
            }
        }
        (lambda, dnu)
    } else {
        (1.0, 1.0)
    };

    let (lambda_y, dnu_y) = if d.sigma_y > 0.0 {
        let (lambda, dnu) = gen_coefficients(d.sigma_y, d.steps);
        for x in 0..d.width {
            for _ in 0..d.steps {
                let mut y = d.width;
                while y < buf.len() {
                    buf[x + y] += dnu * buf[x + y - d.width];
                    y += d.width;
                }
                y = buf.len() - d.width;
                while y > 0 {
                    buf[x + y - d.width] += dnu * buf[x + y];
                    y -= d.width;
                }
            }
        }
        (lambda, dnu)
    } else {
        (1.0, 1.0)
    };

    let post_scale =
        ((dnu_x * dnu_y).sqrt() / (lambda_x * lambda_y).sqrt()).powi(2 * d.steps as i32);
    buf.iter_mut().for_each(|v| *v *= post_scale);
}

fn check_parity(width: u32, height: u32, sigma_x: f64, sigma_y: f64, pixels: &[u8]) {
    let d = BlurData {
        width: width as usize,
        height: height as usize,
        sigma_x,
        sigma_y,
        steps: 4,
    };
    let mut expected = pixels.to_vec();
    for channel in 0..4 {
        let mut original: Vec<f64> = pixels
            .chunks_exact(4)
            .map(|pixel| f64::from(pixel[channel]) / 255.0)
            .collect();
        let mut reordered = original.clone();
        original_gaussianiir2d(&d, &mut original);
        gaussianiir2d(&d, &mut reordered);
        for (i, (&before, &after)) in original.iter().zip(&reordered).enumerate() {
            assert_eq!(
                before.to_bits(),
                after.to_bits(),
                "recurrence changed: {width}x{height}, sigma=({sigma_x}, {sigma_y}), channel={channel}, pixel={i}"
            );
            expected[i * 4 + channel] = (before * 255.0) as u8;
        }
    }

    let mut actual = pixels.to_vec();
    apply(
        sigma_x,
        sigma_y,
        ImageRefMut::new(width, height, bytemuck::cast_slice_mut(&mut actual)),
    );
    assert_eq!(
        actual, expected,
        "RGBA changed: {width}x{height}, sigma=({sigma_x}, {sigma_y})"
    );
}

fn check_sizes_and_sigmas(make_pixels: impl Fn(usize) -> Vec<u8>) {
    for (width, height) in [
        (1, 1),
        (1, 37),
        (41, 1),
        (2, 2),
        (3, 7),
        (31, 23),
        (64, 33),
        (127, 65),
    ] {
        let pixels = make_pixels((width * height) as usize);
        for (sigma_x, sigma_y) in [
            (0.0, 0.0),
            (-1.0, 0.3),
            (0.3, -1.0),
            (0.0, 1.4),
            (1.4, 0.0),
            (0.001, 0.001),
            (0.1, 1.9),
            (1.9, 0.1),
            (0.3, 0.3),
            (1.4, 1.4),
            (1.99, 1.99),
            (1.9999, 1.9999),
            (2.0, 1.4),
            (1.4, 2.0),
            (0.0, 8.0),
            (8.0, 0.0),
        ] {
            // Exercise apply() directly: the SVG Gaussian-blur dispatcher is
            // unchanged and selects other paths for some larger sigmas.
            check_parity(width, height, sigma_x, sigma_y, &pixels);
        }
    }
}

#[test]
fn traversal_preserves_random_premultiplied_rgba_and_recurrence_bits() {
    check_sizes_and_sigmas(|len| {
        let mut state = 0xabc12345_u32;
        let mut next = || {
            state = state.wrapping_mul(1664525).wrapping_add(1013904223);
            (state >> 24) as u8
        };
        let mut pixels = Vec::with_capacity(len * 4);
        for _ in 0..len {
            let alpha = next();
            for _ in 0..3 {
                pixels.push(((u16::from(next()) * u16::from(alpha) + 127) / 255) as u8);
            }
            pixels.push(alpha);
        }
        pixels
    });
}

#[test]
fn traversal_preserves_transparency_constant_fields_and_edge_impulses() {
    check_sizes_and_sigmas(|len| vec![0; len * 4]);
    check_sizes_and_sigmas(|len| [32, 64, 96, 128].repeat(len));
    check_sizes_and_sigmas(|len| {
        let mut pixels = vec![0; len * 4];
        for i in [0, len / 2, len - 1] {
            pixels[i * 4..i * 4 + 4].copy_from_slice(&[64, 32, 0, 128]);
        }
        pixels
    });
}
