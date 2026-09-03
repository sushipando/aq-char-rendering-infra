//! Correctness tests: the SIMD kernels must be byte-identical to the scalar
//! reference on short buffers, edge-case alpha values, and randomized inputs.

use super::scalar::{premultiply_rgba_scalar, source_over_scalar, unpremultiply_rgba_scalar};
use super::wide::{premultiply_rgba, source_over, unpremultiply_rgba};

/// Alpha values that stress rounding boundaries in the divide-by-255 and
/// reciprocal-table math.
const EDGE_ALPHAS: [u8; 10] = [0, 1, 2, 3, 127, 128, 129, 253, 254, 255];

/// Deterministic pseudo-random generator so failures are reproducible.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        // xorshift64
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn byte(&mut self) -> u8 {
        (self.next() >> 32) as u8
    }
}

/// Random straight-alpha RGBA bytes (channels may exceed alpha; that is the
/// decoder's input format).
fn random_straight_rgba(rng: &mut Lcg, pixels: usize) -> Vec<u8> {
    (0..pixels * 4).map(|_| rng.byte()).collect()
}

/// Random *valid* premultiplied RGBA bytes: every channel `<= alpha`.
fn random_premultiplied_rgba(rng: &mut Lcg, pixels: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(pixels * 4);
    for _ in 0..pixels {
        let a = rng.byte();
        out.push(rng.byte().min(a));
        out.push(rng.byte().min(a));
        out.push(rng.byte().min(a));
        out.push(a);
    }
    out
}

/// Random premultiplied RGBA whose alphas come from the edge set, so the
/// reciprocal table and rounding boundaries are exercised heavily.
fn random_edge_alpha_premultiplied(rng: &mut Lcg, pixels: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(pixels * 4);
    for _ in 0..pixels {
        let a = EDGE_ALPHAS[(rng.next() % EDGE_ALPHAS.len() as u64) as usize];
        out.push(rng.byte().min(a));
        out.push(rng.byte().min(a));
        out.push(rng.byte().min(a));
        out.push(a);
    }
    out
}

fn assert_same(a: &[u8], b: &[u8], what: &str) {
    assert_eq!(
        a.len(),
        b.len(),
        "{what}: length mismatch ({} vs {})",
        a.len(),
        b.len()
    );
    for (i, (x, y)) in a.iter().zip(b.iter()).enumerate() {
        assert_eq!(x, y, "{what}: byte {i} differs (0x{x:02x} vs 0x{y:02x})");
    }
}

// ---- source_over ----------------------------------------------------------

#[test]
fn source_over_matches_scalar_short_buffers() {
    // 1..=4 pixels: exercises the scalar tail and the first SIMD block.
    for pixels in 1..=4usize {
        let mut rng = Lcg(0x1234_5678_9abc_def0);
        let src = random_premultiplied_rgba(&mut rng, pixels);
        let dst = random_premultiplied_rgba(&mut rng, pixels);

        let mut scalar = dst.clone();
        let mut simd = dst;
        source_over_scalar(&mut scalar, &src);
        source_over(&mut simd, &src);
        assert_same(&scalar, &simd, &format!("source_over {pixels}px"));
    }
}

#[test]
fn source_over_matches_scalar_edge_alphas() {
    // Every pair of edge alphas, one pixel each, plus a 16-byte block of
    // each single edge alpha (fast-path and rounding coverage).
    let mut cases: Vec<Vec<u8>> = Vec::new();
    for &sa in &EDGE_ALPHAS {
        for &da in &EDGE_ALPHAS {
            cases.push(vec![sa, 0, 0, sa, 0, 0, 0, da]);
        }
        cases.push(vec![sa; 16]); // 4 pixels all with alpha sa
    }
    for (i, src) in cases.iter().enumerate() {
        let mut dst = vec![0u8; src.len()];
        for (j, b) in dst.iter_mut().enumerate() {
            *b = (j * 37 % 256) as u8;
        }
        let mut scalar = dst.clone();
        let mut simd = dst;
        source_over_scalar(&mut scalar, src);
        source_over(&mut simd, src);
        assert_same(&scalar, &simd, &format!("source_over edge case {i}"));
    }
}

#[test]
fn source_over_matches_scalar_random() {
    for pixels in [16, 17, 31, 32, 33, 100, 4096] {
        let mut rng = Lcg(0xdead_beef_cafe_0001 + pixels as u64);
        let src = random_premultiplied_rgba(&mut rng, pixels);
        let dst = random_premultiplied_rgba(&mut rng, pixels);

        let mut scalar = dst.clone();
        let mut simd = dst;
        source_over_scalar(&mut scalar, &src);
        source_over(&mut simd, &src);
        assert_same(&scalar, &simd, &format!("source_over random {pixels}px"));
    }
}

#[test]
fn source_over_transparent_and_opaque_fast_paths() {
    // Fully transparent source: dst unchanged. Fully opaque: dst == src.
    let mut rng = Lcg(0x0bad_f00d_0000_0002);
    let src = random_premultiplied_rgba(&mut rng, 64);
    let mut dst = random_premultiplied_rgba(&mut rng, 64);

    let transparent = vec![0u8; src.len()];
    let before = dst.clone();
    source_over(&mut dst, &transparent);
    assert_eq!(dst, before, "transparent source must not touch dst");

    let mut opaque = src.clone();
    for px in opaque.as_chunks_mut::<4>().0.iter_mut() {
        px[3] = 255;
        px[0] = px[0].max(px[3]); // keep valid premultiplied: channel <= alpha
        px[1] = px[1].max(px[3]);
        px[2] = px[2].max(px[3]);
    }
    let mut dst2 = vec![0u8; src.len()];
    source_over(&mut dst2, &opaque);
    assert_eq!(dst2, opaque, "opaque source must replace dst");
}

// ---- premultiply ----------------------------------------------------------

#[test]
fn premultiply_matches_scalar_short_buffers() {
    for pixels in 1..=4usize {
        let mut rng = Lcg(0x1111_2222_3333_4444);
        let buf = random_straight_rgba(&mut rng, pixels);

        let mut scalar = buf.clone();
        let mut simd = buf;
        premultiply_rgba_scalar(&mut scalar);
        premultiply_rgba(&mut simd);
        assert_same(&scalar, &simd, &format!("premultiply {pixels}px"));
    }
}

#[test]
fn premultiply_matches_scalar_edge_alphas() {
    for &a in &EDGE_ALPHAS {
        for c in [0u8, 1, 2, 127, 128, 254, 255] {
            let src = vec![c, c, c, a];
            let mut scalar = src.clone();
            let mut simd = src;
            premultiply_rgba_scalar(&mut scalar);
            premultiply_rgba(&mut simd);
            assert_same(&scalar, &simd, &format!("premultiply a={a} c={c}"));
        }
    }
}

#[test]
fn premultiply_matches_scalar_random() {
    for pixels in [16, 17, 100, 4096] {
        let mut rng = Lcg(0x5555_6666_7777_8888 + pixels as u64);
        let buf = random_straight_rgba(&mut rng, pixels);

        let mut scalar = buf.clone();
        let mut simd = buf;
        premultiply_rgba_scalar(&mut scalar);
        premultiply_rgba(&mut simd);
        assert_same(&scalar, &simd, &format!("premultiply random {pixels}px"));
    }
}

// ---- unpremultiply --------------------------------------------------------

#[test]
fn unpremultiply_matches_scalar_short_buffers() {
    for pixels in 1..=4usize {
        let mut rng = Lcg(0x9999_aaaa_bbbb_cccc);
        let buf = random_premultiplied_rgba(&mut rng, pixels);

        let mut scalar = buf.clone();
        let mut simd = buf;
        unpremultiply_rgba_scalar(&mut scalar);
        unpremultiply_rgba(&mut simd);
        assert_same(&scalar, &simd, &format!("unpremultiply {pixels}px"));
    }
}

#[test]
fn unpremultiply_matches_scalar_edge_alphas() {
    // Every edge alpha with every channel value 0..=255 exercises the full
    // reciprocal table gather for the boundary alphas.
    for &a in &EDGE_ALPHAS {
        for c in 0..=255u8 {
            let src = vec![c, c, c, a];
            let mut scalar = src.clone();
            let mut simd = src;
            unpremultiply_rgba_scalar(&mut scalar);
            unpremultiply_rgba(&mut simd);
            assert_same(&scalar, &simd, &format!("unpremultiply a={a} c={c}"));
        }
    }
}

#[test]
fn unpremultiply_matches_scalar_random() {
    for pixels in [16, 17, 100, 4096] {
        let mut rng = Lcg(0xdddd_eeee_ffff_0001 + pixels as u64);
        let buf = random_edge_alpha_premultiplied(&mut rng, pixels);

        let mut scalar = buf.clone();
        let mut simd = buf;
        unpremultiply_rgba_scalar(&mut scalar);
        unpremultiply_rgba(&mut simd);
        assert_same(&scalar, &simd, &format!("unpremultiply random {pixels}px"));
    }
}

// ---- full pipeline --------------------------------------------------------

#[test]
fn premultiply_composite_unpremultiply_matches_scalar() {
    // The whole production pipeline: straight layers -> premultiply ->
    // source-over stack -> unpremultiply, SIMD vs scalar end to end.
    let mut rng = Lcg(0x0123_4567_89ab_cdef);
    let mut layers: Vec<Vec<u8>> = Vec::new();
    for _ in 0..5 {
        layers.push(random_straight_rgba(&mut rng, 64));
    }

    let mut scalar_acc = vec![0u8; 64 * 4];
    let mut simd_acc = vec![0u8; 64 * 4];
    for layer in &layers {
        let mut scalar_layer = layer.clone();
        let mut simd_layer = layer.clone();
        premultiply_rgba_scalar(&mut scalar_layer);
        premultiply_rgba(&mut simd_layer);
        source_over_scalar(&mut scalar_acc, &scalar_layer);
        source_over(&mut simd_acc, &simd_layer);
    }
    unpremultiply_rgba_scalar(&mut scalar_acc);
    unpremultiply_rgba(&mut simd_acc);

    assert_same(&scalar_acc, &simd_acc, "full pipeline");
}

// ---- legacy Pillow-exact kernel (reference only) ---------------------------

use super::{blend_pixel, frame_canvas_sizes, py_round, Canvas, RgbaImage};

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

// ---- Canvas geometry / clipping --------------------------------------------

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
    // worker contract that one decode serves every frame in the chunk. The
    // worker premultiplies each layer once after decode.
    let mut shared = layer(3, 3, [120, 30, 200, 200]);
    premultiply_rgba(&mut shared.pixels);
    let mut first = Canvas::new(4, 4);
    let mut second = Canvas::new(4, 4);
    first.composite(&shared, 1, 1);
    second.composite(&shared, 1, 1);
    assert_eq!(first.pixels, second.pixels);
    // Premultiplied: div255(120*200)=94, div255(30*200)=24, div255(200*200)=157.
    assert_eq!(first.pixel(1, 1), [94, 24, 157, 200]);
    assert_eq!(first.pixel(0, 0), [0, 0, 0, 0]);
}

// ---- frame geometry helpers ------------------------------------------------

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
