# Speeding Up Gaussian Blur in a Local `resvg` Package

## Goal

Improve SVG → PNG rendering performance when `feGaussianBlur` is a major bottleneck in `resvg`.

The main idea is to keep `resvg` and `tiny-skia` for normal SVG rendering, but replace or supplement `resvg`'s current blur implementation with a SIMD-accelerated blur implementation such as [`libblur`](https://crates.io/crates/libblur).

This is especially relevant for AWS Lambda, where blur-heavy SVGs can dominate render time.

---

## Why This Is Worth Doing

`tiny-skia` already has SIMD acceleration for many raster operations, but `resvg`'s Gaussian blur path is implemented separately.

That means:

- enabling AVX2/NEON helps normal rasterization;
- it does **not necessarily fix the blur bottleneck**;
- a local `resvg` patch can target blur specifically without rewriting the rest of the renderer.

Current `resvg` behavior is approximately:

```text
feGaussianBlur
      |
      +-- small sigma --> IIR blur
      |
      +-- larger sigma -> box-blur approximation
```

The blur code works on the same premultiplied RGBA buffers used by `tiny-skia`, which makes a replacement relatively straightforward.

---

# Recommended Approach

Start conservatively:

```text
SVG
 |
 v
usvg
 |
 v
resvg + tiny-skia
 |
 v
filter image buffer
 |
 +-- normal filters --> existing resvg implementation
 |
 +-- feGaussianBlur --> libblur
                         |
                         +-- AVX2 on x86_64
                         +-- NEON on AArch64
 |
 v
tiny-skia output
 |
 v
PNG
```

The first implementation should use `libblur`'s regular Gaussian blur with the original SVG sigma values.

Only after validating image output should you test its faster approximate Gaussian implementations.

---

# Step 1: Use a Local `resvg` Fork

Clone or copy the `resvg` source into your project/workspace.

For example:

```text
project/
├── Cargo.toml
├── src/
└── vendor/
    └── resvg/
        ├── crates/
        │   └── resvg/
        └── ...
```

Then point Cargo at your local version.

If your application directly depends on `resvg`:

```toml
[dependencies]
resvg = { path = "vendor/resvg/crates/resvg" }
```

Or override the crate globally:

```toml
[patch.crates-io]
resvg = { path = "vendor/resvg/crates/resvg" }
```

The `[patch.crates-io]` approach is useful if another dependency also depends on `resvg`.

---

# Step 2: Add `libblur` to the Local `resvg`

Inside the local `resvg` crate:

```text
vendor/resvg/crates/resvg/Cargo.toml
```

add:

```toml
[dependencies]
libblur = "0.24"
```

Use the current compatible version in your lockfile if the API has changed.

---

# Step 3: Locate the Existing Blur Dispatch

In `resvg`, find the Gaussian blur filter implementation.

The current structure is roughly:

```rust
if use_box_blur {
    box_blur::apply(...);
} else {
    iir_blur::apply(...);
}
```

This is the point where you want to introduce the alternative implementation.

Do **not** initially delete the existing code.

Keep it behind a fallback or feature flag so you can compare correctness and performance.

For example:

```rust
if use_simd_blur {
    libblur_apply(...);
} else if use_box_blur {
    box_blur::apply(...);
} else {
    iir_blur::apply(...);
}
```

---

# Step 4: Start With Exact/Conservative Gaussian Blur

For the first patch, preserve the SVG's actual X/Y sigma values.

SVG supports:

```xml
<feGaussianBlur stdDeviation="x y" />
```

so the replacement must support anisotropic blur.

Conceptually:

```rust
let params =
    GaussianBlurParams::new_asymmetric_from_sigma(
        std_dx as f64,
        std_dy as f64,
    );
```

Then blur the existing premultiplied RGBA image buffer.

A rough integration looks like:

```rust
use libblur::{
    BlurImage,
    BlurImageMut,
    ConvolutionMode,
    EdgeMode,
    EdgeMode2D,
    FastBlurChannels,
    GaussianBlurParams,
    ThreadingPolicy,
};

fn apply_libblur(
    src_data: &[u8],
    dst_data: &mut [u8],
    width: u32,
    height: u32,
    sigma_x: f32,
    sigma_y: f32,
) -> Result<(), libblur::BlurError> {
    let src = BlurImage::borrow(
        src_data,
        width,
        height,
        FastBlurChannels::Channels4,
    );

    let mut dst = BlurImageMut::borrow(
        dst_data,
        width,
        height,
        FastBlurChannels::Channels4,
    );

    let params =
        GaussianBlurParams::new_asymmetric_from_sigma(
            sigma_x as f64,
            sigma_y as f64,
        );

    libblur::gaussian_blur(
        &src,
        &mut dst,
        params,
        EdgeMode2D::new(EdgeMode::Clamp),
        ThreadingPolicy::Single,
        ConvolutionMode::FixedPoint,
    )
}
```

Treat this as integration pseudocode rather than copy/paste-final code; verify the exact API against the `libblur` version you add.

---

# Step 5: Preserve Premultiplied RGBA

This is important.

`tiny-skia`/`resvg` filter images use premultiplied RGBA.

You want the blur implementation to operate directly on that representation:

```text
R' = R * A
G' = G * A
B' = B * A
A  = A
```

Avoid this:

```text
premultiplied RGBA
    ↓
unpremultiply
    ↓
blur
    ↓
premultiply again
```

That conversion would add substantial overhead and can introduce artifacts around translucent edges.

The ideal path is:

```text
tiny-skia buffer
      ↓
libblur
      ↓
same buffer format
```

---

# Step 6: Correctly Map SVG Edge Behavior

Do not hardcode `Clamp` permanently.

Gaussian blur behavior near filter boundaries matters.

Map SVG/resvg behavior to the closest `libblur` behavior.

Conceptually:

```text
SVG edge behavior       libblur
---------------------------------------
duplicate                Clamp
wrap                     Wrap
none                     Transparent/constant zero
```

The exact API for transparent constant borders should be verified against the `libblur` version being used.

This is one of the most important correctness checks.

A wrong edge mode may look fine in most images but produce halos or smeared edges around clipped filter regions.

---

# Step 7: Add a Feature Flag

Make it easy to switch between old and new implementations.

In the local `resvg` Cargo file:

```toml
[features]
simd-blur = ["dep:libblur"]

[dependencies]
libblur = { version = "0.24", optional = true }
```

Then:

```rust
#[cfg(feature = "simd-blur")]
{
    // libblur implementation
}

#[cfg(not(feature = "simd-blur"))]
{
    // original resvg implementation
}
```

Your application can enable it with:

```toml
resvg = {
    path = "vendor/resvg/crates/resvg",
    features = ["simd-blur"]
}
```

This makes A/B testing much easier.

---

# Step 8: Benchmark the Conservative Version First

Create a benchmark set containing SVGs with blur values such as:

```text
sigma = 0.5
sigma = 1
sigma = 1.5
sigma = 2
sigma = 3
sigma = 5
sigma = 10
sigma = 20
```

Include:

- small filtered regions;
- large filtered regions;
- transparent shadows;
- glows;
- heavily blurred shapes;
- multiple blur filters in one SVG;
- asymmetric blur (`stdDeviation="10 3"`).

Measure at least:

```text
parse time
render time
blur/filter time
PNG encode time
total invocation time
```

The important number is the render/filter portion, not just total Lambda duration.

---

# Step 9: Compare Image Output

For every reference SVG:

1. Render using stock `resvg`.
2. Render using patched `resvg`.
3. Compare the PNGs.

Useful metrics:

```text
maximum per-channel difference
mean absolute error
RMSE
percentage of changed pixels
```

Also visually inspect:

- drop shadows;
- transparent edges;
- glows;
- clipped filters;
- overlapping filters;
- filter regions near image bounds.

For game assets, very small numeric differences may be completely acceptable if the image is visually identical.

---

# Step 10: Try `ConvolutionMode::FixedPoint`

Once the basic patch works, compare the different convolution modes supported by `libblur`.

A likely production candidate is:

```rust
ConvolutionMode::FixedPoint
```

This trades a small amount of numerical precision for higher throughput.

Test:

```text
exact/reference Gaussian
vs
fixed-point Gaussian
```

If the output difference is visually negligible, use fixed-point.

---

# Step 11: Test `fast_gaussian_next`

After the exact-sigma implementation is validated, test the faster approximation.

Conceptually:

```rust
libblur::fast_gaussian_next(...)
```

This is more aggressive and should be treated as a separate optimization.

The major complication is that it may use a blur **radius** rather than the exact SVG Gaussian sigma.

Do not assume:

```rust
radius = sigma as u32;
```

Instead, derive an empirical mapping.

For example:

```text
SVG sigma
   ↓
candidate radius
   ↓
render
   ↓
compare against stock resvg
   ↓
pick closest match
```

Test across:

```text
sigma = 0.5
1
1.5
2
3
5
10
20
```

Then derive a conversion function if the relationship is consistent.

---

# Possible Final Strategy

A good optimized implementation may end up looking like:

```text
sigma < threshold
        |
        v
libblur Gaussian + FixedPoint

sigma >= threshold
        |
        v
libblur fast_gaussian_next
```

For example:

```rust
if sigma_x.max(sigma_y) < FAST_BLUR_THRESHOLD {
    gaussian_fixed_point(...);
} else {
    fast_gaussian(...);
}
```

Do not choose the threshold until you have benchmarks and visual-difference tests.

---

# AWS Lambda SIMD Build Settings

## x86_64 Lambda

Compile for an AVX2-capable baseline:

```bash
RUSTFLAGS="-C target-cpu=haswell" \
cargo lambda build --release --x86-64
```

This allows LLVM and supporting crates to use a more capable x86 instruction set.

`libblur` can also runtime-dispatch to AVX2 where supported.

Recommended release profile:

```toml
[profile.release]
opt-level = 3
lto = true
codegen-units = 1
```

---

## ARM64 Lambda

Build with:

```bash
cargo lambda build --release --arm64
```

AArch64 provides NEON SIMD as part of the architecture, so explicit x86-style AVX2 flags are not needed.

Benchmark both architectures with your actual SVG workload.

Do not assume x86_64 or ARM64 will automatically be faster.

---

# Keep Blur Single-Threaded Initially

Start with:

```rust
ThreadingPolicy::Single
```

SIMD and multithreading are separate optimizations.

This is preferable if:

- each Lambda has approximately one vCPU;
- you already fan out rendering across Lambda invocations;
- you render multiple SVGs concurrently elsewhere;
- you want deterministic A/B benchmarks.

After SIMD performance is known, test multi-threading only on Lambda configurations with additional vCPUs.

---

# Other Blur Optimizations That May Matter More Than SIMD

SIMD is useful, but the amount of image data being blurred can matter even more.

## Minimize filter surfaces

Blur cost scales heavily with the size of the intermediate image.

For example:

```text
400 × 400
= 160,000 pixels
```

versus:

```text
2000 × 2000
= 4,000,000 pixels
```

That is 25× as many pixels.

If possible, avoid oversized SVG filter regions such as:

```xml
<filter x="-500%" y="-500%" width="1000%" height="1000%">
```

when the effect only needs a small margin.

---

## Avoid unnecessary high-resolution rendering

Rendering at 2× width and height gives approximately:

```text
2 × width
2 × height
----------------
4 × pixels
```

All blur work then operates over roughly four times as much image data.

Only supersample when the final quality requires it.

---

# Recommended Development Sequence

Do the work in this order:

```text
1. Vendor/fork resvg locally
        ↓
2. Add libblur dependency
        ↓
3. Replace feGaussianBlur with regular libblur Gaussian
        ↓
4. Match SVG edge behavior
        ↓
5. Validate PNG output
        ↓
6. Benchmark stock vs patched
        ↓
7. Enable FixedPoint mode
        ↓
8. Benchmark again
        ↓
9. Test fast_gaussian_next
        ↓
10. Derive sigma → radius mapping
        ↓
11. Benchmark x86_64 AVX2 vs ARM64 NEON
        ↓
12. Deploy the fastest visually-correct implementation
```

---

# Suggested A/B Architecture

Keep all three implementations during development:

```rust
enum BlurBackend {
    ResvgOriginal,
    LibblurGaussian,
    LibblurFast,
}
```

Then allow an environment variable:

```text
RESVG_BLUR_BACKEND=original
RESVG_BLUR_BACKEND=libblur
RESVG_BLUR_BACKEND=fast
```

This makes Lambda testing easy without changing the rest of the rendering pipeline.

---

# Success Criteria

The patch is successful if:

```text
✓ SVG output remains visually equivalent
✓ alpha edges remain correct
✓ filter boundaries remain correct
✓ asymmetric blur works
✓ average render latency decreases
✓ p95/p99 latency decreases
✓ Lambda CPU time decreases
✓ no crashes on either x86_64 or ARM64
```

For this workload, prioritize **p95/p99 render latency**, not just average latency, because blur-heavy assets are likely to create the slow tail.

---

# Recommended First Version

Do not start by rewriting the entire blur subsystem.

Implement only this:

```text
stock resvg
    ↓
intercept feGaussianBlur
    ↓
libblur::gaussian_blur
    ↓
ConvolutionMode::FixedPoint
    ↓
ThreadingPolicy::Single
```

Compile x86 Lambda with:

```bash
RUSTFLAGS="-C target-cpu=haswell"
```

Then benchmark.

If that produces a meaningful improvement with acceptable image differences, move on to `fast_gaussian_next`.

That gives the best balance of:

```text
low integration risk
+
SIMD acceleration
+
preserved SVG semantics
+
easy rollback
```
