//! Scalar reference implementations for the premultiplied-RGBA compositor.
//!
//! These are the permanent correctness oracle: every SIMD kernel in
//! [`super::wide`] must produce byte-identical output, and `tests.rs`
//! enforces that. The scalar kernels are also used for the tail pixels that
//! do not fill a full 16-byte SIMD block.
//!
//! The compositor works in **premultiplied RGBA8**:
//!
//! ```text
//! R' = round(R * A / 255)
//! ```
//!
//! so a 50% transparent red pixel `(255, 0, 0, 128)` becomes `(128, 0, 0,
//! 128)`. Porter-Duff source-over then has no per-pixel division by the
//! output alpha:
//!
//! ```text
//! inv_a   = 255 - src.a
//! out.c   = src.c + round(dst.c * inv_a / 255)
//! out.a   = src.a + round(dst.a * inv_a / 255)
//! ```

/// Rounded divide by 255 using only shifts and adds.
///
/// `div255(x) == (x + 127) / 255` (round-half-up) for every `x` in
/// `0..=255*255`, which is the full range the compositor ever produces.
/// This is what lets the SIMD path avoid general integer division.
#[inline(always)]
pub fn div255(x: u16) -> u16 {
    let t = x + 128;
    (t + (t >> 8)) >> 8
}

/// Fixed-point reciprocal table used by unpremultiply.
///
/// `recip[a] = round(255 * 256 / a)` for `a` in `1..=255`, `recip[0] = 0`.
/// The value is stored as high/low byte planes so the SIMD path can gather
/// them with byte shuffles and keep all arithmetic in `u16`:
///
/// ```text
/// out = round(c * 255 / a)
///     = (c * recip[a] + 128) >> 8
///     = c * rh[a] + ((c * rl[a] + 128) >> 8)     // recip = 256*rh + rl
/// ```
///
/// The last form never exceeds `u16` (`c * rh <= 255*255`, plus at most
/// 255), so no widening beyond `u16` is needed.
pub(crate) const RECIP_RH: [u8; 256] = build_recip_rh();
pub(crate) const RECIP_RL: [u8; 256] = build_recip_rl();

const fn recip(a: usize) -> u16 {
    // `checked_div` documents the a == 0 -> 0 rule (alpha 0 maps to black).
    match (255 * 256 + a / 2).checked_div(a) {
        Some(value) => value as u16,
        None => 0,
    }
}

const fn build_recip_rh() -> [u8; 256] {
    let mut out = [0u8; 256];
    let mut a = 0;
    while a < 256 {
        out[a] = (recip(a) >> 8) as u8;
        a += 1;
    }
    out
}

const fn build_recip_rl() -> [u8; 256] {
    let mut out = [0u8; 256];
    let mut a = 0;
    while a < 256 {
        out[a] = (recip(a) & 0xff) as u8;
        a += 1;
    }
    out
}

/// Porter-Duff source-over on premultiplied RGBA8 buffers.
///
/// `src` is composited over `dst` in place. Both buffers must be the same
/// length, a multiple of 4 bytes.
pub fn source_over_scalar(dst: &mut [u8], src: &[u8]) {
    assert_eq!(dst.len(), src.len());
    assert_eq!(dst.len() % 4, 0);

    for (d, s) in dst
        .as_chunks_mut::<4>()
        .0
        .iter_mut()
        .zip(src.as_chunks::<4>().0.iter())
    {
        let sa = s[3] as u16;
        let inv_a = 255 - sa;

        d[0] = (s[0] as u16 + div255(d[0] as u16 * inv_a)).min(255) as u8;
        d[1] = (s[1] as u16 + div255(d[1] as u16 * inv_a)).min(255) as u8;
        d[2] = (s[2] as u16 + div255(d[2] as u16 * inv_a)).min(255) as u8;
        d[3] = (sa + div255(d[3] as u16 * inv_a)).min(255) as u8;
    }
}

/// Convert straight-alpha RGBA8 to premultiplied RGBA8 in place.
pub fn premultiply_rgba_scalar(buf: &mut [u8]) {
    assert_eq!(buf.len() % 4, 0);

    for px in buf.as_chunks_mut::<4>().0.iter_mut() {
        let a = px[3] as u16;
        px[0] = div255(px[0] as u16 * a) as u8;
        px[1] = div255(px[1] as u16 * a) as u8;
        px[2] = div255(px[2] as u16 * a) as u8;
    }
}

/// Convert premultiplied RGBA8 back to straight-alpha RGBA8 in place.
///
/// Uses the same fixed-point reciprocal table as the SIMD path so the two
/// implementations agree byte-for-byte. Fully transparent pixels (alpha 0)
/// map to black, matching the conventional unpremultiply behavior.
pub fn unpremultiply_rgba_scalar(buf: &mut [u8]) {
    assert_eq!(buf.len() % 4, 0);

    for px in buf.as_chunks_mut::<4>().0.iter_mut() {
        let a = px[3] as usize;
        let rh = RECIP_RH[a] as u16;
        let rl = RECIP_RL[a] as u16;
        px[0] = unpremul_channel(px[0], rh, rl);
        px[1] = unpremul_channel(px[1], rh, rl);
        px[2] = unpremul_channel(px[2], rh, rl);
    }
}

#[inline(always)]
fn unpremul_channel(c: u8, rh: u16, rl: u16) -> u8 {
    let c = c as u16;
    ((c * rh + ((c * rl + 128) >> 8)).min(255)) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn div255_is_rounded_division() {
        // Exhaustive over the full u16 range the kernel can produce.
        for x in 0..=255 * 255 {
            assert_eq!(div255(x), (x + 127) / 255, "div255({x})");
        }
    }

    #[test]
    fn source_over_never_overflows_255() {
        // For valid premultiplied inputs (channel <= alpha) the result must
        // stay in range, so the SIMD path can use a plain narrow.
        for sa in 0..=255u16 {
            for da in 0..=255u16 {
                let inv = 255 - sa;
                let out = sa + div255(da * inv);
                assert!(out <= 255, "sa={sa} da={da} out={out}");
            }
        }
    }

    #[test]
    fn premultiply_round_trip_keeps_alpha() {
        // Alpha must survive premultiply -> unpremultiply untouched.
        for c in 0..=255u8 {
            for a in 0..=255u8 {
                let mut px = [c, c, c, a];
                premultiply_rgba_scalar(&mut px);
                assert_eq!(px[3], a);
                unpremultiply_rgba_scalar(&mut px);
                assert_eq!(px[3], a);
            }
        }
    }

    #[test]
    fn unpremultiply_alpha_zero_is_black() {
        let mut px = [200, 100, 50, 0];
        unpremultiply_rgba_scalar(&mut px);
        assert_eq!(px, [0, 0, 0, 0]);
    }

    #[test]
    fn unpremultiply_opaque_is_identity() {
        for c in 0..=255u8 {
            let mut px = [c, c, c, 255];
            unpremultiply_rgba_scalar(&mut px);
            assert_eq!(px, [c, c, c, 255]);
        }
    }
}
