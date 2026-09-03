# SIMD RGBA Layer Compositing with `wide` on AWS Lambda ARM64

## Goal

Speed up the second Lambda in the AQW rendering pipeline.

This Lambda receives already-rasterized item layers for a character and stacks them in AQW/Flash z-order to produce each final animation frame.

The operation is **Porter-Duff source-over alpha compositing**:

```text
cape raster ───────┐
armor raster ──────┤
head raster ───────┤
hair raster ───────┤
weapon raster ─────┤
                   ▼
           final RGBA frame
```

Because this hot loop is extremely small, we do not need ThorVG, Skia, tiny-skia, or Pixman just to perform the layer compositing.

Use the Rust [`wide`](https://crates.io/crates/wide) crate to express the operation with SIMD-friendly vector types.

For this project we only target:

```text
AWS Lambda ARM64 / Graviton
        ↓
AArch64
        ↓
NEON / Advanced SIMD
```

There is no need to support x86 or AVX.

---

# 1. Internal Pixel Format: Premultiplied RGBA8

The compositor should operate on **premultiplied RGBA8**.

Straight-alpha RGBA for a 50% transparent red pixel:

```text
255, 0, 0, 128
```

Premultiplied RGBA:

```text
128, 0, 0, 128
```

because:

```text
128 ≈ 255 * 128 / 255
```

For premultiplied alpha, source-over becomes:

```text
inv_a = 255 - src.a

out.r = src.r + dst.r * inv_a / 255
out.g = src.g + dst.g * inv_a / 255
out.b = src.b + dst.b * inv_a / 255
out.a = src.a + dst.a * inv_a / 255
```

This is much nicer for SIMD than straight-alpha blending.

The desired pipeline is:

```text
decoded item raster
        ↓
premultiplied RGBA8
        ↓
SIMD source-over
        ↓
next layer
        ↓
SIMD source-over
        ↓
...
        ↓
final premultiplied RGBA8
        ↓
unpremultiply once if required by encoder
        ↓
PNG / WebP
```

Do not premultiply/unpremultiply between every layer.

---

# 2. Add `wide`

```toml
[dependencies]
wide = "1.7"
```

Pin the final exact version in `Cargo.lock`.

Recommended release settings:

```toml
[profile.release]
opt-level = 3
lto = true
codegen-units = 1
panic = "abort"
```

---

# 3. Build Only for ARM64 Lambda

Build with:

```bash
cargo lambda build --release --arm64
```

AArch64 has NEON/Advanced SIMD as part of the normal architecture baseline.

Therefore the project does not need:

```text
runtime AVX detection
SSE fallbacks
x86 feature dispatch
target-cpu=haswell
```

The deployment target is simply:

```text
aarch64-unknown-linux-gnu
```

or whichever AArch64 Lambda target your existing build setup uses.

---

# 4. First Implement a Scalar Reference

Before SIMD, create a scalar implementation that becomes the permanent correctness oracle.

```rust
#[inline(always)]
fn div255(x: u16) -> u16 {
    let t = x + 128;
    (t + (t >> 8)) >> 8
}

pub fn source_over_scalar(
    dst: &mut [u8],
    src: &[u8],
) {
    assert_eq!(dst.len(), src.len());
    assert_eq!(dst.len() % 4, 0);

    for (d, s) in dst
        .chunks_exact_mut(4)
        .zip(src.chunks_exact(4))
    {
        let sa = s[3] as u16;
        let inv_a = 255 - sa;

        // Both src and dst are premultiplied RGBA8.

        d[0] =
            (s[0] as u16
                + div255(d[0] as u16 * inv_a))
            as u8;

        d[1] =
            (s[1] as u16
                + div255(d[1] as u16 * inv_a))
            as u8;

        d[2] =
            (s[2] as u16
                + div255(d[2] as u16 * inv_a))
            as u8;

        d[3] =
            (sa
                + div255(d[3] as u16 * inv_a))
            as u8;
    }
}
```

This is the reference result the SIMD implementation must match.

---

# 5. Why `div255` Uses Shifts

Do not put general integer division inside the SIMD hot loop.

Instead of:

```rust
x / 255
```

use:

```rust
let t = x + 128;
(t + (t >> 8)) >> 8
```

For the integer range used by RGBA8 source-over, this gives the desired rounded divide-by-255 behavior.

That turns division into:

```text
ADD
SHIFT
ADD
SHIFT
```

which maps much better to SIMD instructions.

---

# 6. SIMD Pixel Layout

RGBA8 is interleaved:

```text
R0 G0 B0 A0
R1 G1 B1 A1
R2 G2 B2 A2
R3 G3 B3 A3
```

A 128-bit SIMD vector can hold:

```text
16 bytes
=
4 RGBA pixels
```

Conceptually:

```text
u8x16:

R0 G0 B0 A0 R1 G1 B1 A1 R2 G2 B2 A2 R3 G3 B3 A3
```

To blend those pixels, first replicate alpha:

```text
A0 A0 A0 A0
A1 A1 A1 A1
A2 A2 A2 A2
A3 A3 A3 A3
```

then compute:

```text
inverse alpha
=
255 - alpha
```

for all channels.

---

# 7. SIMD Algorithm

For each block:

```text
LOAD src RGBA bytes
LOAD dst RGBA bytes

        ↓

extract/replicate src alpha

        ↓

inv_alpha = 255 - alpha

        ↓

widen src:
u8 → u16

widen dst:
u8 → u16

widen inv_alpha:
u8 → u16

        ↓

dst_scaled =
    dst * inv_alpha

        ↓

dst_scaled =
    dst_scaled / 255

        ↓

out =
    src + dst_scaled

        ↓

narrow:
u16 → u8

        ↓

STORE dst
```

Widening is necessary because:

```text
255 * 255 = 65025
```

does not fit in `u8`.

---

# 8. Rough `wide` Implementation

The final `wide` API details for byte shuffling and narrowing should be matched to the version pinned in the project.

The compositor should roughly look like this:

```rust
use wide::{u8x16, u16x16};

#[inline(always)]
fn div255_vec(x: u16x16) -> u16x16 {
    let t = x + u16x16::splat(128);
    (t + (t >> 8)) >> 8
}

pub fn source_over_simd(
    dst: &mut [u8],
    src: &[u8],
) {
    assert_eq!(dst.len(), src.len());
    assert_eq!(dst.len() % 4, 0);

    const BLOCK_BYTES: usize = 16;

    let simd_len =
        dst.len() / BLOCK_BYTES * BLOCK_BYTES;

    let (dst_simd, dst_tail) =
        dst.split_at_mut(simd_len);

    let (src_simd, src_tail) =
        src.split_at(simd_len);

    for (dst_block, src_block) in
        dst_simd
            .chunks_exact_mut(BLOCK_BYTES)
            .zip(
                src_simd
                    .chunks_exact(BLOCK_BYTES)
            )
    {
        let src8 =
            load_u8x16(src_block);

        let dst8 =
            load_u8x16(dst_block);

        /*
         * Input:
         *
         * R0 G0 B0 A0
         * R1 G1 B1 A1
         * R2 G2 B2 A2
         * R3 G3 B3 A3
         *
         * Output:
         *
         * A0 A0 A0 A0
         * A1 A1 A1 A1
         * A2 A2 A2 A2
         * A3 A3 A3 A3
         */
        let alpha8 =
            replicate_rgba_alpha(src8);

        let inv_alpha8 =
            u8x16::splat(255) - alpha8;

        /*
         * Widen before multiplication.
         *
         * Depending on the exact wide API,
         * this may produce low/high halves
         * rather than one u16x16.
         *
         * Keep the widening logic isolated.
         */
        let src16 =
            widen_u8_to_u16(src8);

        let dst16 =
            widen_u8_to_u16(dst8);

        let inv_alpha16 =
            widen_u8_to_u16(inv_alpha8);

        let scaled_dst =
            div255_vec(
                dst16 * inv_alpha16
            );

        let out16 =
            src16 + scaled_dst;

        let out8 =
            narrow_u16_to_u8(out16);

        store_u8x16(
            dst_block,
            out8,
        );
    }

    // Any remaining pixels use the
    // trusted scalar implementation.
    source_over_scalar(
        dst_tail,
        src_tail,
    );
}
```

The key helpers are intentionally isolated:

```rust
load_u8x16(...)
store_u8x16(...)
replicate_rgba_alpha(...)
widen_u8_to_u16(...)
narrow_u16_to_u8(...)
```

That keeps SIMD mechanics out of the main compositing logic.

---

# 9. Alpha Replication

For:

```text
R0 G0 B0 A0
R1 G1 B1 A1
R2 G2 B2 A2
R3 G3 B3 A3
```

we need:

```text
A0 A0 A0 A0
A1 A1 A1 A1
A2 A2 A2 A2
A3 A3 A3 A3
```

Conceptually:

```rust
#[inline(always)]
fn replicate_rgba_alpha(
    rgba: u8x16,
) -> u8x16 {
    /*
     * Conceptual byte indices:
     *
     *  3,  3,  3,  3,
     *  7,  7,  7,  7,
     * 11, 11, 11, 11,
     * 15, 15, 15, 15
     *
     * Implement using the shuffle/swizzle
     * operation provided by the pinned
     * version of `wide`.
     */
    todo!()
}
```

On ARM64 this should ultimately map to an efficient NEON shuffle/table operation or equivalent generated sequence.

---

# 10. Narrowing

After the computation, values are held in wider integer lanes.

Conceptually:

```rust
#[inline(always)]
fn narrow_u16_to_u8(
    value: u16x16,
) -> u8x16 {
    /*
     * Use the packing/narrowing operation
     * offered by the pinned `wide` version.
     *
     * Valid premultiplied source-over
     * results should remain <= 255.
     */
    todo!()
}
```

During early development it is fine to write a slow scalar helper for this operation purely to establish correctness.

Replace it with SIMD packing before final benchmarking.

---

# 11. Fast Paths

There are two useful fast paths.

## Fully Transparent Source Block

If all source alphas are zero:

```text
src alpha = 0
```

then:

```text
dst stays unchanged
```

So:

```rust
if all_source_alphas_zero(src8) {
    continue;
}
```

No arithmetic and no destination write are necessary.

---

## Fully Opaque Source Block

If all source alphas are 255:

```text
src alpha = 255
```

then:

```text
dst = src
```

So:

```rust
if all_source_alphas_255(src8) {
    dst_block.copy_from_slice(
        src_block
    );

    continue;
}
```

Benchmark these branches on actual AQW layers.

They may help considerably because character assets often contain both:

```text
large fully-transparent areas
+
large fully-opaque painted areas
```

---

# 12. Crop Transparent Bounds Before Compositing

This may save more CPU than SIMD itself.

Do not store or composite this:

```text
512 x 512 raster

┌─────────────────────────┐
│                         │
│           sword         │
│                         │
│                         │
└─────────────────────────┘
```

if the actual non-transparent portion is only:

```text
80 x 220
```

Instead store:

```rust
pub struct RasterLayer {
    pub x: u32,
    pub y: u32,

    pub width: u32,
    pub height: u32,

    // Premultiplied RGBA8.
    pub pixels: Vec<u8>,
}
```

Then blend only those pixels.

---

# 13. Row-Based Layer Compositor

```rust
pub fn composite_layer(
    frame: &mut [u8],
    frame_width: usize,
    layer: &RasterLayer,
) {
    let frame_stride =
        frame_width * 4;

    let layer_stride =
        layer.width as usize * 4;

    for row in 0..layer.height as usize {
        let src_start =
            row * layer_stride;

        let src_end =
            src_start + layer_stride;

        let dst_y =
            layer.y as usize + row;

        let dst_x =
            layer.x as usize;

        let dst_start =
            dst_y * frame_stride
                + dst_x * 4;

        let dst_end =
            dst_start + layer_stride;

        source_over_simd(
            &mut frame[
                dst_start..dst_end
            ],
            &layer.pixels[
                src_start..src_end
            ],
        );
    }
}
```

The SIMD kernel therefore always works on contiguous runs of pixels.

---

# 14. Final Frame Function

```rust
pub fn render_frame(
    width: usize,
    height: usize,
    layers: &[RasterLayer],
) -> Vec<u8> {
    let mut frame =
        vec![0u8; width * height * 4];

    // Layers are already ordered by
    // Flash/AQW depth.
    for layer in layers {
        composite_layer(
            &mut frame,
            width,
            layer,
        );
    }

    frame
}
```

This is essentially the entire renderer for the second Lambda.

---

# 15. Premultiplying Decoded Input

If the input decoder returns straight-alpha RGBA:

```rust
pub fn premultiply_rgba(
    buf: &mut [u8],
) {
    for px in buf.chunks_exact_mut(4) {
        let a =
            px[3] as u16;

        px[0] =
            div255(
                px[0] as u16 * a
            ) as u8;

        px[1] =
            div255(
                px[1] as u16 * a
            ) as u8;

        px[2] =
            div255(
                px[2] as u16 * a
            ) as u8;
    }
}
```

This can also be SIMD-optimized later.

Do not optimize it until profiling shows that it matters.

---

# 16. Unpremultiplying Final Output

PNG normally expects straight/unassociated alpha.

If the encoder requires straight RGBA, convert once at the end.

Conceptually:

```rust
if a == 0 {
    r = 0;
    g = 0;
    b = 0;
} else {
    r = min(
        255,
        premul_r * 255 / a
    );

    g = min(
        255,
        premul_g * 255 / a
    );

    b = min(
        255,
        premul_b * 255 / a
    );
}
```

Do this:

```text
once per final frame
```

not:

```text
once per item layer
```

because division by varying alpha values is relatively expensive.

---

# 17. Suggested Module Layout

```text
src/
├── compositor/
│   ├── mod.rs
│   ├── scalar.rs
│   ├── neon_wide.rs
│   └── tests.rs
├── decode.rs
├── encode.rs
└── main.rs
```

`compositor/mod.rs` might expose:

```rust
mod scalar;
mod neon_wide;

pub use neon_wide::source_over_simd;
```

The rest of the application should never know about SIMD vector types.

Public API:

```rust
pub fn source_over(
    dst_premul_rgba: &mut [u8],
    src_premul_rgba: &[u8],
);
```

and:

```rust
pub fn composite_layer(
    frame: &mut [u8],
    frame_width: usize,
    layer: &RasterLayer,
);
```

---

# 18. Correctness Testing

The SIMD implementation must match the scalar implementation.

Generate random **valid premultiplied RGBA** buffers.

For every pixel:

```text
R <= A
G <= A
B <= A
```

Then compare:

```rust
#[test]
fn simd_matches_scalar() {
    let src =
        make_random_premultiplied_rgba(
            4096
        );

    let original_dst =
        make_random_premultiplied_rgba(
            4096
        );

    let mut scalar_dst =
        original_dst.clone();

    let mut simd_dst =
        original_dst;

    source_over_scalar(
        &mut scalar_dst,
        &src,
    );

    source_over_simd(
        &mut simd_dst,
        &src,
    );

    assert_eq!(
        scalar_dst,
        simd_dst
    );
}
```

Pay particular attention to alpha values:

```text
0
1
2
127
128
253
254
255
```

---

# 19. Image-Level Regression Tests

For several real AQW characters:

```text
old compositor
      ↓
reference frame

new wide compositor
      ↓
new frame
```

Compare:

```text
exact pixel bytes
max channel difference
number of changed pixels
```

The goal should be **byte-identical source-over results** if both implementations use the same rounding rules.

---

# 20. Benchmark the Right Things

Benchmark the compositing kernel separately from:

```text
S3 download
PNG/WebP decode
premultiply
final encode
S3 upload
```

Test:

```text
512 x 512
1024 x 1024

5 layers
10 layers
20 layers

cropped layers
full-canvas layers

transparent-heavy
opaque-heavy
mixed-alpha
```

Measure:

```text
ns / pixel
pixels / second
frame compositing time

p50
p95
p99
```

Then measure the complete Lambda.

---

# 21. Expect Memory Bandwidth to Become the Bottleneck

Once the arithmetic is SIMD-optimized, each layer mostly does:

```text
read src
read dst
write dst
```

For a 512x512 RGBA8 image:

```text
512 * 512 * 4
≈ 1 MiB
```

A full-canvas blend therefore moves several MiB of memory.

After SIMD works, reducing pixels touched often matters more than improving the math further.

That means:

```text
cropped bounding boxes
```

are especially important.

---

# 22. Optional Block Metadata

If cropped layers still contain large transparent regions, split them into small blocks, for example:

```text
16 x 16 pixels
```

and classify each block:

```text
transparent
opaque
mixed
```

Then:

```text
transparent
    ↓
skip

opaque
    ↓
copy

mixed
    ↓
SIMD source-over
```

Do not implement this until profiling demonstrates the normal SIMD compositor is still significant.

---

# 23. Recommended Implementation Order

```text
1. Standardize on premultiplied RGBA8
        ↓
2. Implement scalar source-over
        ↓
3. Add randomized correctness tests
        ↓
4. Add `wide`
        ↓
5. Process 4 RGBA pixels per SIMD block
        ↓
6. Implement alpha replication
        ↓
7. Widen u8 → u16
        ↓
8. Multiply dst by inverse alpha
        ↓
9. Divide by 255 using shifts/adds
        ↓
10. Add src
        ↓
11. SIMD-narrow u16 → u8
        ↓
12. Scalar tail
        ↓
13. Verify byte-for-byte against scalar
        ↓
14. Add transparent/opaque fast paths
        ↓
15. Benchmark on actual Lambda ARM64
```

---

# 24. If `wide` Is Not Fast Enough

The next step is **not** another graphics library.

Since the deployment is ARM64-only, the natural next optimization would be a tiny explicit NEON implementation using:

```rust
std::arch::aarch64
```

For example:

```text
vld1q_u8
        ↓
NEON byte shuffle / table lookup
        ↓
vmovl_u8
        ↓
vmulq_u16
        ↓
shift/add divide-by-255
        ↓
vaddq_u16
        ↓
vqmovn_u16
        ↓
vst1q_u8
```

That gives complete control over the generated SIMD sequence.

But do not start there.

First determine whether `wide` already makes the compositing stage insignificant.

---

# Final Target Architecture

```text
Rasterized AQW layers
        ↓
decode
        ↓
premultiplied RGBA8
        ↓
cropped layer bounds
        ↓
┌─────────────────────────────┐
│ ARM64 SIMD source-over      │
│                             │
│ `wide` → AArch64 NEON       │
└─────────────────────────────┘
        ↓
final premultiplied RGBA
        ↓
unpremultiply if necessary
        ↓
encode final PNG/WebP once
```

The actual hot loop should ultimately reduce to:

```rust
for layer in layers {
    for row in layer.rows() {
        source_over_simd(
            frame_row,
            layer_row,
        );
    }
}
```

No general graphics engine is necessary.

The first milestone is not maximum theoretical NEON throughput. It is:

> Make the SIMD implementation byte-for-byte equivalent to the scalar reference, benchmark it on actual Graviton Lambda hardware, and then determine whether compositing remains important enough to optimize further.
