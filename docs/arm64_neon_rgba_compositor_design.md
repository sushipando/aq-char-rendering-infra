# ARM64 SIMD RGBA Compositor Design for AWS Lambda

## Goal

Build a **fast and correct** RGBA layer compositor for the second AQW Lambda.

This Lambda receives already-rasterized item layers and stacks them in Flash/AQW z-order to produce the final animation frame.

The key operation is **Porter-Duff source-over alpha compositing**.

The target platform is:

```text
AWS Lambda
   ↓
ARM64 / Graviton
   ↓
AArch64 NEON
```

Because the deployment is ARM64-only, we have two reasonable implementation paths:

```text
1. `wide` first
   ↓
   simpler Rust implementation
   ↓
   benchmark
   ↓
   good enough? keep it

2. direct `std::arch::aarch64`
   ↓
   lower-level NEON implementation
   ↓
   only if `wide` leaves meaningful performance on the table
```

This document now recommends:

> **Use `wide` first as the implementation vehicle, while using Pixman as the main algorithm/reference design.**
>
> Keep the direct NEON path as the next step only if profiling shows `wide` is not sufficient.

---

## Recommendation

Use a **premultiplied RGBA8** internal representation.

Implementation strategy:

```text
Pixman
    ↓
algorithm / SIMD structure reference

Skia
    ↓
premultiplied-alpha semantics reference

fast_image_resize
    ↓
premultiply / unpremultiply reference

our Rust code
    ↓
try `wide` first
    ↓
if needed, replace hot path with std::arch::aarch64 NEON
```

The target should be:

```text
correct Porter-Duff source-over
+
premultiplied RGBA
+
SIMD
+
minimal memory traffic
```

Unless bit-identical Pillow output is required, there is no need to preserve Pillow's exact historical arithmetic.

---

## Why Premultiplied RGBA

Straight RGBA stores RGB independently of alpha:

```text
255 0 0 128
```

for a 50% transparent red pixel.

Premultiplied RGBA stores:

```text
R' = R * A / 255
G' = G * A / 255
B' = B * A / 255
```

so the same pixel becomes approximately:

```text
128 0 0 128
```

For premultiplied RGBA, source-over becomes:

```text
inv_a = 255 - src.a

out.r = src.r + dst.r * inv_a / 255
out.g = src.g + dst.g * inv_a / 255
out.b = src.b + dst.b * inv_a / 255
out.a = src.a + dst.a * inv_a / 255
```

This is substantially easier to SIMD than straight-alpha compositing because there is no per-pixel divide by output alpha.

This is also the conventional internal representation used by major graphics engines such as Skia.

---

## Main Inspiration: Pixman

Pixman's ARM NEON compositor is the main implementation blueprint.

Its optimized `OVER 8888 -> 8888` kernel essentially does:

```text
load RGBA pixels
       ↓
deinterleave channels
       ↓
use source alpha
       ↓
inv_alpha = 255 - src_alpha
       ↓
widen destination channels to u16
       ↓
destination * inv_alpha
       ↓
rounded divide by 255
       ↓
add premultiplied source
       ↓
saturating narrow back to u8
       ↓
interleave + store
```

This is nearly identical to the compositor required by the AQW Lambda.

The useful design lesson is:

> Keep the working pixels in premultiplied RGBA8 and do the entire blend with integer SIMD operations.

We are **not** required to literally copy Pixman's assembly.
We are using Pixman mainly as:

```text
algorithm reference
+
lane-layout inspiration
+
proof that this architecture is production-worthy
```

---

## Why `wide` First

`wide` gives a nicer Rust implementation for the first pass.

Advantages:

```text
clearer code
easier debugging
less error-prone than raw intrinsics
still SIMD-friendly
lets us validate the algorithm quickly
```

The arithmetic from the Pixman-style source-over kernel maps well to `wide`:

```text
subtract
multiply
add
shift
narrow
```

That means we can do:

```text
Pixman algorithm
        ↓
translate to `wide`
        ↓
benchmark on Lambda ARM64
```

If the result is already fast enough, stop there.

Only move to direct `std::arch::aarch64` if:

```text
compositing is still materially expensive
or
assembly inspection shows poor lowering
or
shuffles/narrowing become the bottleneck
```

So the intended development order is:

```text
scalar reference
    ↓
`wide` implementation
    ↓
benchmark
    ↓
if needed:
direct NEON implementation
```

---

## Why We Still Keep the Pixman Material

Even if `wide` is used first, Pixman remains the best reference for:

```text
how many pixels to process per block
how to structure premultiplied OVER
how to think about deinterleave / alpha / widen / divide-by-255 / add / pack
what a mature CPU compositor converges to
```

So the final implementation path is:

```text
Pixman ideas
     ↓
`wide` code first
     ↓
optional direct NEON later
```

---

## Suggested Pixel Pipeline

```text
PNG/WebP layer
      ↓
decode to RGBA8
      ↓
premultiply once
      ↓
cropped premultiplied RGBA8 layer
      ↓
SIMD source-over
      ↓
SIMD source-over
      ↓
SIMD source-over
      ↓
...
      ↓
final premultiplied RGBA8
      ↓
unpremultiply once if encoder requires straight alpha
      ↓
final PNG/WebP
```

Do not repeatedly convert between straight and premultiplied alpha.

---

## Reference Scalar Formula

Implement the scalar version first.

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

        d[0] =
            (s[0] as u16
                + div255(d[0] as u16 * inv_a))
            .min(255) as u8;

        d[1] =
            (s[1] as u16
                + div255(d[1] as u16 * inv_a))
            .min(255) as u8;

        d[2] =
            (s[2] as u16
                + div255(d[2] as u16 * inv_a))
            .min(255) as u8;

        d[3] =
            (sa
                + div255(d[3] as u16 * inv_a))
            .min(255) as u8;
    }
}
```

This becomes the correctness oracle for the SIMD implementations.

---

## Why Divide-by-255 Uses Shifts

Do not use normal integer division in the hot path.

Instead of:

```rust
x / 255
```

use:

```rust
let t = x + 128;
(t + (t >> 8)) >> 8
```

That turns the operation into:

```text
add
shift
add
shift
```

which maps much better to SIMD.

---

## `wide` Implementation Shape

The first SIMD implementation should use `wide`.

### Dependency

```toml
[dependencies]
wide = "1.7"
```

### Conceptual block structure

RGBA8 is interleaved:

```text
R0 G0 B0 A0 R1 G1 B1 A1 R2 G2 B2 A2 R3 G3 B3 A3
```

A natural first implementation is to process 16 bytes at a time:

```text
4 RGBA pixels per u8x16 block
```

You can also benchmark larger effective blocks by unrolling later, but do not start there.

### Rough structure

```rust
use wide::{u8x16, u16x16};

#[inline(always)]
fn div255_vec(x: u16x16) -> u16x16 {
    let t = x + u16x16::splat(128);
    (t + (t >> 8)) >> 8
}

pub fn source_over_wide(
    dst: &mut [u8],
    src: &[u8],
) {
    assert_eq!(dst.len(), src.len());
    assert_eq!(dst.len() % 4, 0);

    const BLOCK_BYTES: usize = 16;
    let simd_len = dst.len() / BLOCK_BYTES * BLOCK_BYTES;

    let (dst_simd, dst_tail) = dst.split_at_mut(simd_len);
    let (src_simd, src_tail) = src.split_at(simd_len);

    for (dst_block, src_block) in dst_simd
        .chunks_exact_mut(BLOCK_BYTES)
        .zip(src_simd.chunks_exact(BLOCK_BYTES))
    {
        let src8 = u8x16::from(src_block);
        let dst8 = u8x16::from(&*dst_block);

        let alpha8 = replicate_rgba_alpha(src8);
        let inv_alpha8 = u8x16::splat(255) - alpha8;

        let src16 = widen_u8_to_u16(src8);
        let dst16 = widen_u8_to_u16(dst8);
        let inv16 = widen_u8_to_u16(inv_alpha8);

        let scaled_dst = div255_vec(dst16 * inv16);
        let out16 = src16 + scaled_dst;

        let out8 = narrow_u16_to_u8(out16);

        dst_block.copy_from_slice(&out8.to_array());
    }

    source_over_scalar(dst_tail, src_tail);
}
```

The exact `wide` helpers may differ depending on the pinned version:

```text
replicate_rgba_alpha(...)
widen_u8_to_u16(...)
narrow_u16_to_u8(...)
```

The important point is that the **algorithm** still follows the Pixman blueprint.

---

## `wide` vs Direct NEON

The two paths should coexist cleanly.

Public API:

```rust
pub fn source_over(
    dst: &mut [u8],
    src: &[u8],
)
```

Internally:

```text
compositor/
    scalar.rs
    wide.rs
    neon.rs
```

Start with:

```rust
pub use wide::source_over_wide as source_over;
```

If needed later, switch to:

```rust
pub use neon::source_over_neon as source_over;
```

The rest of the codebase should not need to change.

---

## When to Escalate to Direct NEON

Only move past `wide` if benchmarking justifies it.

Good reasons:

```text
1. compositing remains a major portion of Lambda time
2. assembly inspection shows bad codegen
3. byte shuffles / packing are inefficient
4. direct NEON would remove obvious overhead
```

At that point, use:

```rust
std::arch::aarch64
```

for the hot loop while keeping the same scalar reference and same public API.

---

## Direct NEON Implementation Shape (Second Step, If Needed)

If `wide` is not enough, the next implementation should be a direct AArch64 NEON loop inspired by Pixman.

Conceptually:

```rust
use std::arch::aarch64::*;

#[target_feature(enable = "neon")]
unsafe fn source_over_neon(
    dst: &mut [u8],
    src: &[u8],
) {
    assert_eq!(dst.len(), src.len());
    assert_eq!(dst.len() % 4, 0);

    let mut i = 0;

    while i + 32 <= src.len() {
        let s = vld4_u8(src.as_ptr().add(i));
        let d = vld4_u8(dst.as_ptr().add(i));

        let inv_a = vmvn_u8(s.3);

        let dr16 = vmull_u8(d.0, inv_a);
        let dg16 = vmull_u8(d.1, inv_a);
        let db16 = vmull_u8(d.2, inv_a);
        let da16 = vmull_u8(d.3, inv_a);

        let dr = div255_neon(dr16);
        let dg = div255_neon(dg16);
        let db = div255_neon(db16);
        let da = div255_neon(da16);

        let out_r = vqadd_u8(s.0, dr);
        let out_g = vqadd_u8(s.1, dg);
        let out_b = vqadd_u8(s.2, db);
        let out_a = vqadd_u8(s.3, da);

        let out = uint8x8x4_t(out_r, out_g, out_b, out_a);
        vst4_u8(dst.as_mut_ptr().add(i), out);

        i += 32;
    }

    source_over_scalar(&mut dst[i..], &src[i..]);
}
```

This remains a **fallback improvement step**, not the first implementation target.

---

## Fast Paths

These ideas apply whether the implementation is `wide` or direct NEON.

### Transparent source block

If all source alpha values are zero:

```text
dst remains unchanged
```

### Opaque source block

If all source alpha values are 255:

```text
dst = src
```

These can be worthwhile, but benchmark them on the real AQW workload.

---

## Cropping Is More Important Than Micro-Optimizing

If a layer only occupies:

```text
80 x 220
```

pixels, do not composite a full:

```text
512 x 512
```

buffer.

Store each raster layer as:

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

Then composite only the relevant rows.

---

## Row-Based Compositor

```rust
pub fn composite_layer(
    frame: &mut [u8],
    frame_width: usize,
    layer: &RasterLayer,
) {
    let frame_stride = frame_width * 4;
    let layer_stride = layer.width as usize * 4;

    for row in 0..layer.height as usize {
        let src_start = row * layer_stride;
        let src_end = src_start + layer_stride;

        let dst_y = layer.y as usize + row;

        let dst_start =
            dst_y * frame_stride
            + layer.x as usize * 4;

        let dst_end =
            dst_start + layer_stride;

        source_over(
            &mut frame[dst_start..dst_end],
            &layer.pixels[src_start..src_end],
        );
    }
}
```

The SIMD kernel therefore operates on contiguous rows.

---

## Runtime API

The rest of the application should not know whether `wide` or raw NEON is used.

Expose:

```rust
pub fn source_over(
    dst_premul_rgba: &mut [u8],
    src_premul_rgba: &[u8],
);
```

A scalar function should remain for:

```text
tests
debugging
correctness validation
tail handling
```

---

## Premultiplication Reference: `fast_image_resize`

`fast_image_resize` is still a useful Rust reference for the conversion stages around the compositor.

Study or reuse its ARM64-friendly implementations for:

```text
straight RGBA8
      ↓
premultiplied RGBA8
```

and:

```text
premultiplied RGBA8
      ↓
straight RGBA8
```

The main compositor itself should remain your tiny specialized source-over kernel.

---

## Why Not `f32`

A premultiplied floating-point implementation is mathematically clean and SIMD handles `f32` well.

But:

```text
RGBA8 = 4 bytes / pixel
RGBA f32 = 16 bytes / pixel
```

The compositor is likely to become memory-bandwidth-bound after SIMD optimization.

Using float would approximately quadruple pixel memory traffic.

For a simple source-over stack, prefer:

```text
premultiplied RGBA8
+
integer SIMD
```

unless profiling demonstrates a real correctness requirement that forces higher precision.

---

## Why Not Pillow Math

Pillow composites straight RGBA and preserves straight-alpha output after every operation.

That requires more fixed-point arithmetic, including division by output alpha.

If bit-identical Pillow output is not required, there is no reason to retain that more complicated representation.

Use standard premultiplied Porter-Duff compositing instead.

---

## Correctness Test Strategy

Generate random **valid premultiplied** RGBA pixels.

For valid premultiplied pixels:

```text
R <= A
G <= A
B <= A
```

Then compare scalar vs `wide`, and later scalar vs NEON if implemented.

```rust
#[test]
fn wide_matches_scalar() {
    let src = make_random_premultiplied_rgba(100_000);
    let initial_dst = make_random_premultiplied_rgba(100_000);

    let mut scalar = initial_dst.clone();
    let mut wide_out = initial_dst;

    source_over_scalar(&mut scalar, &src);
    source_over_wide(&mut wide_out, &src);

    assert_eq!(scalar, wide_out);
}
```

If a NEON path is later added, it should be validated the same way.

Test alpha values heavily around:

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

## Image-Level Regression Testing

Use real AQW frames.

Compare:

```text
existing compositor
vs
new premultiplied SIMD compositor
```

Measure:

```text
max channel difference
mean absolute difference
RMSE
changed pixel count
```

If migrating from Pillow's straight-alpha arithmetic, exact byte equality is not necessarily expected.

The important criteria are:

```text
correct Porter-Duff output
no halos
no dark fringes
no transparent-edge artifacts
no alpha corruption
```

---

## Performance Benchmarking

Benchmark these separately:

```text
decode
premultiply
composite
unpremultiply
encode
```

For compositing, test:

```text
512x512
1024x1024

5 layers
10 layers
20 layers

full-canvas
cropped

mostly transparent
mostly opaque
mixed-alpha
```

Measure:

```text
ns / pixel
pixels / second
frame time
p50
p95
p99
```

Also inspect whether compositing remains visible at all after the `wide` implementation.

---

## Expect Memory Bandwidth to Become the Limit

Once the kernel is efficient, each layer mostly does:

```text
read source
read destination
write destination
```

For a 512x512 RGBA8 frame:

```text
512 * 512 * 4
≈ 1 MiB
```

After SIMD is working, the largest remaining wins will probably come from:

```text
cropping
reducing decode work
avoiding redundant copies
avoiding repeated premultiply/unpremultiply
reducing intermediate image I/O
```

rather than adding more arithmetic tricks.

---

## Optional Future Optimization: Block Classification

If cropped images still contain large transparent/opaque areas, classify small blocks:

```text
16 x 16 pixels
```

as:

```text
transparent
opaque
mixed
```

Then:

```text
transparent -> skip
opaque      -> memcpy
mixed       -> SIMD source-over
```

Do not implement this until profiling shows the plain cropped SIMD compositor still matters.

---

## Suggested Module Layout

```text
src/
├── compositor/
│   ├── mod.rs
│   ├── scalar.rs
│   ├── wide.rs
│   ├── neon.rs
│   └── tests.rs
├── decode.rs
├── premultiply.rs
├── encode.rs
└── main.rs
```

Keep the scalar reference permanent.
Keep `wide` as the initial production path.
Keep `neon.rs` reserved for a future lower-level optimization if needed.

---

## Recommended Implementation Sequence

```text
1. Standardize compositor on premultiplied RGBA8
        ↓
2. Implement scalar source-over
        ↓
3. Add randomized correctness tests
        ↓
4. Implement `wide` source-over
        ↓
5. Match scalar byte-for-byte
        ↓
6. Add scalar tail handling
        ↓
7. Benchmark on Lambda ARM64
        ↓
8. Add transparent block fast path if useful
        ↓
9. Add opaque block fast path if useful
        ↓
10. Benchmark again
        ↓
11. Only if needed: implement direct NEON version
        ↓
12. Swap backend without changing public API
```

---

## Final Architecture

```text
AQW raster layers
       ↓
decode
       ↓
premultiply RGBA8
       ↓
crop transparent bounds
       ↓
┌─────────────────────────────┐
│ SIMD source-over            │
│                             │
│ first: `wide`               │
│ later if needed: raw NEON   │
│ guided by Pixman structure  │
└─────────────────────────────┘
       ↓
final premultiplied frame
       ↓
unpremultiply if required
       ↓
final PNG/WebP encode
```

The central principle is:

> Do not build a general renderer for this Lambda. Implement the one operation the Lambda actually needs, use Pixman as the reference design, and try `wide` first before dropping to raw NEON intrinsics.

---

## Reference Projects

### Pixman

Best reference for:

```text
premultiplied RGBA8
+
integer source-over
+
ARM SIMD implementation structure
```

Canonical project:

https://gitlab.freedesktop.org/pixman/pixman

### Skia

Best reference for:

```text
premultiplied-alpha semantics
source-over behavior
production raster architecture
```

https://skia.org/

### fast_image_resize

Useful Rust reference for:

```text
RGBA8 handling
premultiply
unpremultiply
SIMD-friendly implementation ideas
```

https://github.com/Cykooz/fast_image_resize

### ThorVG

Useful reference for:

```text
lightweight CPU renderer design
ARM SIMD
production compositing architecture
```

https://github.com/thorvg/thorvg

### tiny-skia

Useful Rust reference for:

```text
premultiplied RGBA
Porter-Duff semantics
small software renderer architecture
```

https://github.com/linebender/tiny-skia

### `wide`

Primary first implementation vehicle for the SIMD compositor:

https://github.com/Lokathor/wide
https://docs.rs/wide/
