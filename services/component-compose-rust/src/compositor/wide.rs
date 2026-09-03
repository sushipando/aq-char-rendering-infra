//! SIMD implementations of the premultiplied-RGBA compositor kernels.
//!
//! Built on the [`wide`] crate, which lowers to AArch64 NEON on the Graviton
//! Lambda target. Every function here must produce byte-identical output to
//! its scalar counterpart in [`super::scalar`]; `tests.rs` enforces that.
//!
//! Each 128-bit block holds 4 interleaved RGBA pixels:
//!
//! ```text
//! R0 G0 B0 A0 R1 G1 B1 A1 R2 G2 B2 A2 R3 G3 B3 A3
//! ```
//!
//! The kernels widen to `u16` before multiplying (255*255 does not fit in
//! `u8`), divide by 255 with the shift/add trick, and saturating-narrow back
//! to `u8`. Pixels that do not fill a full block fall through to the scalar
//! reference.

use wide::{i16x8, u16x16, u8x16};

use super::scalar::{source_over_scalar, unpremultiply_rgba_scalar};

/// Bytes per SIMD block: 4 RGBA pixels.
const BLOCK_BYTES: usize = 16;

/// Byte indices that replicate each pixel's alpha across its four channels:
/// `A0 A0 A0 A0 A1 A1 A1 A1 A2 A2 A2 A2 A3 A3 A3 A3`.
const ALPHA_INDICES: [u8; 16] = [3, 3, 3, 3, 7, 7, 7, 7, 11, 11, 11, 11, 15, 15, 15, 15];

/// Replicate the alpha channel of each of the 4 pixels in a block.
///
/// On AArch64 this lowers to a single `vqtbl1q_u8` table lookup.
#[inline(always)]
fn replicate_rgba_alpha(rgba: u8x16) -> u8x16 {
    rgba.shuffle(u8x16::new(ALPHA_INDICES))
}

/// Rounded divide by 255 for all 16 lanes: `(x + 128 + ((x + 128) >> 8)) >> 8`.
#[inline(always)]
fn div255_vec(x: u16x16) -> u16x16 {
    let t = x + 128;
    (t + (t >> 8u32)) >> 8u32
}

/// Saturating narrow `u16x16` -> `u8x16`.
///
/// Source-over results are always `<= 255` for valid premultiplied inputs,
/// so the saturation is a safety net rather than a rounding choice.
#[inline(always)]
fn narrow_u16_to_u8(value: u16x16) -> u8x16 {
    let arr = value.cast_signed().to_array();
    u8x16::narrow_i16x8(
        i16x8::new([
            arr[0], arr[1], arr[2], arr[3], arr[4], arr[5], arr[6], arr[7],
        ]),
        i16x8::new([
            arr[8], arr[9], arr[10], arr[11], arr[12], arr[13], arr[14], arr[15],
        ]),
    )
}

/// Porter-Duff source-over on premultiplied RGBA8 buffers (SIMD + scalar tail).
///
/// `src` is composited over `dst` in place. Both buffers must be the same
/// length, a multiple of 4 bytes.
pub fn source_over(dst: &mut [u8], src: &[u8]) {
    assert_eq!(dst.len(), src.len());
    assert_eq!(dst.len() % 4, 0);

    let simd_len = dst.len() / BLOCK_BYTES * BLOCK_BYTES;
    let (dst_simd, dst_tail) = dst.split_at_mut(simd_len);
    let (src_simd, src_tail) = src.split_at(simd_len);

    for (dst_block, src_block) in dst_simd
        .as_chunks_mut::<BLOCK_BYTES>()
        .0
        .iter_mut()
        .zip(src_simd.as_chunks::<BLOCK_BYTES>().0.iter())
    {
        let src8 = u8x16::from(&src_block[..]);
        let dst8 = u8x16::from(&dst_block[..]);

        let alpha8 = replicate_rgba_alpha(src8);

        // Fast paths: fully transparent source leaves dst untouched; fully
        // opaque source replaces dst. AQW layers contain large runs of both.
        if alpha8 == u8x16::splat(0) {
            continue;
        }
        if alpha8 == u8x16::splat(255) {
            dst_block.copy_from_slice(src_block);
            continue;
        }

        let inv_alpha8 = u8x16::splat(255) - alpha8;

        let src16 = u16x16::from(src8);
        let dst16 = u16x16::from(dst8);
        let inv16 = u16x16::from(inv_alpha8);

        let scaled_dst = div255_vec(dst16 * inv16);
        let out16 = src16 + scaled_dst;

        let out8 = narrow_u16_to_u8(out16);
        dst_block.copy_from_slice(&out8.to_array());
    }

    source_over_scalar(dst_tail, src_tail);
}

/// u16 lane mask: `0xFFFF` for RGB lanes, `0x0000` for alpha lanes.
const RGB_MASK16: u16x16 = u16x16::new([
    0xffff, 0xffff, 0xffff, 0x0000, 0xffff, 0xffff, 0xffff, 0x0000, 0xffff, 0xffff, 0xffff, 0x0000,
    0xffff, 0xffff, 0xffff, 0x0000,
]);

/// Convert straight-alpha RGBA8 to premultiplied RGBA8 in place (SIMD).
pub fn premultiply_rgba(buf: &mut [u8]) {
    assert_eq!(buf.len() % 4, 0);

    let simd_len = buf.len() / BLOCK_BYTES * BLOCK_BYTES;
    let (simd, tail) = buf.split_at_mut(simd_len);

    for block in simd.as_chunks_mut::<BLOCK_BYTES>().0.iter_mut() {
        let px8 = u8x16::from(&block[..]);
        let alpha8 = replicate_rgba_alpha(px8);

        let px16 = u16x16::from(px8);
        let alpha16 = u16x16::from(alpha8);

        // Multiply RGB by alpha; keep the alpha channel untouched.
        let out16 = (div255_vec(px16 * alpha16) & RGB_MASK16) | (px16 & !RGB_MASK16);
        let out8 = narrow_u16_to_u8(out16);

        block.copy_from_slice(&out8.to_array());
    }

    super::scalar::premultiply_rgba_scalar(tail);
}

// ---- unpremultiply --------------------------------------------------------
//
// `out = round(c * 255 / a)` needs a division by a per-pixel variable alpha.
// `wide` has no integer->float conversion and no integer division, so we use
// a fixed-point reciprocal table gathered with byte shuffles:
//
//   recip[a] = round(255 * 256 / a)          (a in 1..=255, recip[0] = 0)
//   out      = (c * recip[a] + 128) >> 8
//            = c * rh[a] + ((c * rl[a] + 128) >> 8)
//
// where rh/rl are the high/low bytes of recip. The last form stays in u16.
// The table is split into 16-entry vectors; `shuffle` (vqtbl1q_u8 on NEON)
// gathers `table[off]` and a per-lane select on `chunk = a >> 4` picks the
// right 16-entry slice. The scalar reference uses the identical table, so
// the two paths agree byte-for-byte.

const RECIP_RH_TABLES: [u8x16; 16] = build_recip_tables(&super::scalar::RECIP_RH);
const RECIP_RL_TABLES: [u8x16; 16] = build_recip_tables(&super::scalar::RECIP_RL);

const fn build_recip_tables(table: &[u8; 256]) -> [u8x16; 16] {
    let mut out = [u8x16::splat(0); 16];
    let mut k = 0;
    while k < 16 {
        let mut bytes = [0u8; 16];
        let mut i = 0;
        while i < 16 {
            bytes[i] = table[k * 16 + i];
            i += 1;
        }
        out[k] = u8x16::new(bytes);
        k += 1;
    }
    out
}

/// Gather `table[alpha]` for 16 alphas at once.
///
/// `chunk = alpha >> 4` selects one of the 16 table vectors; `off = alpha &
/// 15` is the in-vector index. Each candidate is gathered with one shuffle
/// and the winner is picked with bitselects.
#[inline(always)]
fn gather_recip(tables: &[u8x16; 16], chunk: u8x16, off: u8x16) -> u8x16 {
    #[allow(deprecated)] // wide's CmpEq::simd_eq is the mask primitive here
    {
        let mut acc = tables[0].shuffle(off);
        let mut k = 1;
        while k < 16 {
            let mask = chunk.simd_eq(u8x16::splat(k as u8));
            let candidate = tables[k].shuffle(off);
            // bitselect(self, if_one, if_zero): self is the mask.
            acc = mask.bitselect(candidate, acc);
            k += 1;
        }
        acc
    }
}

/// Convert premultiplied RGBA8 back to straight-alpha RGBA8 in place (SIMD).
pub fn unpremultiply_rgba(buf: &mut [u8]) {
    assert_eq!(buf.len() % 4, 0);

    let simd_len = buf.len() / BLOCK_BYTES * BLOCK_BYTES;
    let (simd, tail) = buf.split_at_mut(simd_len);

    for block in simd.as_chunks_mut::<BLOCK_BYTES>().0.iter_mut() {
        let px8 = u8x16::from(&block[..]);
        let alpha8 = replicate_rgba_alpha(px8);

        let off = alpha8 & u8x16::splat(15);
        let chunk = alpha8 >> 4u32;

        let rh8 = gather_recip(&RECIP_RH_TABLES, chunk, off);
        let rl8 = gather_recip(&RECIP_RL_TABLES, chunk, off);

        let c16 = u16x16::from(px8);
        let rh16 = u16x16::from(rh8);
        let rl16 = u16x16::from(rl8);

        let out16 =
            (c16 * rh16 + ((c16 * rl16 + u16x16::splat(128)) >> 8u32)).min(u16x16::splat(255));
        // Unpremultiply only RGB; keep the alpha channel untouched.
        let out16 = (out16 & RGB_MASK16) | (c16 & !RGB_MASK16);
        let out8 = narrow_u16_to_u8(out16);

        block.copy_from_slice(&out8.to_array());
    }

    unpremultiply_rgba_scalar(tail);
}
