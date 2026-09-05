# resvg 0.48.1 + libblur Gaussian Blur Patch

For **resvg 0.48.1**, the recommended fix is to use **libblur's `gaussian_box_blur` for the same large-sigma cases where upstream resvg already chooses its box-Gaussian approximation**.

The current patch sends *every* Gaussian through:

```rust
libblur::gaussian_blur(..., ConvolutionMode::Exact)
```

before resvg can choose its large-sigma box path.

That is the core performance problem:

- `libblur::gaussian_blur(... Exact)` is an analytical Gaussian convolution with cost that grows with blur radius.
- `gaussian_box_blur` is a three-box Gaussian approximation with effectively constant-time behavior with respect to radius.
- Upstream resvg already chooses an approximate box-blur path for large sigma, so `gaussian_box_blur` is much closer to resvg's intended performance model.

---

## 1. Cargo.toml

If using current libblur:

```toml
[features]
simd-blur = ["dep:libblur"]

[dependencies.libblur]
version = "0.24"
optional = true
```

For an ARM-only Lambda build, explicitly enable the ARM SIMD paths:

```toml
[dependencies.libblur]
version = "0.24"
optional = true
default-features = false
features = ["neon", "rdm"]
```

---

## 2. Replace `apply_blur()`

Use this implementation:

```rust
fn apply_blur(
    fe: &usvg::filter::GaussianBlur,
    cs: usvg::filter::ColorInterpolation,
    ts: usvg::Transform,
    input: Image,
) -> Result<Image, Error> {
    let (std_dx, std_dy, use_box_blur) =
        match resolve_std_dev(fe.std_dev_x().get(), fe.std_dev_y().get(), ts) {
            Some(v) => v,
            None => return Ok(input),
        };

    // Do color-space conversion first, but DO NOT call take().
    //
    // This matters because a referenced filter result is Rc-backed.
    // Calling take() here can deep-clone an entire filter pixmap.
    let input = input.into_color_space(cs)?;

    #[cfg(feature = "simd-blur")]
    {
        let backend =
            std::env::var("RESVG_BLUR_BACKEND").unwrap_or_else(|_| "libblur".into());

        // Upstream resvg chooses its box-Gaussian approximation for large
        // sigma. Do the same thing, but through SIMD libblur.
        //
        // Require both axes >= 2 for now. This avoids changing semantics
        // for cases like stdDeviation="0 10", which libblur's CLT path
        // isn't a clean drop-in for.
        if backend != "original"
            && use_box_blur
            && std_dx >= 2.0
            && std_dy >= 2.0
        {
            if let Some(pixmap) =
                blur_via_libblur_box(input.as_ref(), std_dx as f32, std_dy as f32)
            {
                return Ok(Image::from_image(pixmap, cs));
            }
        }
    }

    // Only request exclusive ownership if we're falling back to upstream
    // resvg's in-place implementations.
    let mut pixmap = input.take()?;

    if use_box_blur {
        box_blur::apply(std_dx, std_dy, pixmap.as_image_ref_mut());
    } else {
        iir_blur::apply(std_dx, std_dy, pixmap.as_image_ref_mut());
    }

    Ok(Image::from_image(pixmap, cs))
}
```

### Why moving `take()` matters

The current `Image` stores its pixmap in:

```rust
Rc<tiny_skia::Pixmap>
```

and `take()` does essentially:

```rust
match Rc::try_unwrap(self.image) {
    Ok(v) => Ok(v),
    Err(v) => Ok((*v).clone()),
}
```

For named filter references such as:

```text
blur0 -> blur1 -> blur2
```

`get_input()` clones the `Image`, which increments the `Rc`. The prior result is still stored in the filter results vector.

Therefore `take()` cannot unwrap the `Rc` and performs a full pixmap clone.

For libblur's out-of-place path, that clone is unnecessary. Borrow the input read-only and allocate only the destination.

---

## 3. Replace `blur_via_libblur()`

Delete the current exact-Gaussian wrapper and use:

```rust
#[cfg(feature = "simd-blur")]
fn blur_via_libblur_box(
    pixmap: &tiny_skia::Pixmap,
    std_dx: f32,
    std_dy: f32,
) -> Option<tiny_skia::Pixmap> {
    use libblur::{
        BlurImage,
        BlurImageMut,
        CLTParameters,
        FastBlurChannels,
        ThreadingPolicy,
    };

    let width = pixmap.width();
    let height = pixmap.height();

    let len = (width as usize)
        .checked_mul(height as usize)?
        .checked_mul(4)?;

    // Only ONE wrapper-owned destination allocation.
    //
    // The input is borrowed directly from tiny-skia. No Pixmap -> Vec copy.
    let mut dst_bytes = Vec::<u8>::new();

    if dst_bytes.try_reserve_exact(len).is_err() {
        log::warn!(
            "libblur blur of {}x{} too large; falling back to resvg blur",
            width,
            height
        );
        return None;
    }

    dst_bytes.resize(len, 0);

    {
        // tiny-skia Pixmap is already tightly-packed premultiplied RGBA8,
        // which is what libblur Channels4 expects.
        let src = BlurImage::borrow(
            pixmap.data(),
            width,
            height,
            FastBlurChannels::Channels4,
        );

        let mut dst = BlurImageMut::borrow(
            &mut dst_bytes,
            width,
            height,
            FastBlurChannels::Channels4,
        );

        // gaussian_box_blur takes sigma directly, so this maps naturally
        // to SVG stdDeviation after resvg applies the world transform.
        let params = CLTParameters {
            x_sigma: std_dx,
            y_sigma: std_dy,
        };

        let threading = match std::env::var("RESVG_BLUR_THREADS").as_deref() {
            Ok("adaptive") => ThreadingPolicy::Adaptive,
            _ => ThreadingPolicy::Single,
        };

        if libblur::gaussian_box_blur(
            &src,
            &mut dst,
            params,
            threading,
        )
        .is_err()
        {
            log::warn!("libblur gaussian_box_blur failed; falling back to resvg blur");
            return None;
        }
    }

    // Transfer ownership of dst_bytes directly into tiny-skia.
    // No Vec -> Pixmap pixel-by-pixel copy.
    let size = tiny_skia::IntSize::from_wh(width, height)?;

    tiny_skia::Pixmap::from_vec(dst_bytes, size)
}
```

---

## 4. What the current patch is doing

The current implementation effectively does:

```text
Rc<Pixmap>
    |
    +-- deep copy from take()        potentially tens of MB
    |
    v
Pixmap
    |
    +-- copy every RGBA pixel
    v
src Vec<u8>
    |
    v
Exact Gaussian O(radius)
    |
    v
scratch Vec<u8>
    |
    +-- copy every RGBA pixel
    v
Pixmap
```

The wrapper allocates both `src` and `scratch`, copies the whole pixmap into `src`, runs the analytical Gaussian, and then copies `scratch` back into the pixmap.

That creates substantial memory traffic before and after the blur itself.

---

## 5. What the replacement does

The new path becomes:

```text
Rc<Pixmap>
    |
    | borrowed read-only
    v
libblur gaussian_box_blur
    |
    v
Vec<u8>
    |
    | ownership transfer
    v
Pixmap
```

This removes:

- the `Rc` deep clone on the successful libblur path
- the Pixmap -> `src Vec<u8>` copy
- the `scratch Vec<u8>` -> Pixmap copy
- analytical Gaussian convolution for large sigma

There will still be internal working storage inside the multipass blur implementation, but the adapter itself stops duplicating entire images unnecessarily.

---

## 6. Why `gaussian_box_blur` instead of `fast_gaussian_next`

`fast_gaussian_next` is attractive, but it takes an integer blur radius.

SVG gives you:

```text
stdDeviation = sigma
```

so using `fast_gaussian_next` as a drop-in replacement requires establishing and validating a sigma-to-radius mapping.

`gaussian_box_blur`, on the other hand, takes:

```rust
CLTParameters {
    x_sigma,
    y_sigma,
}
```

directly.

That means the transformed SVG sigma values coming out of:

```rust
resolve_std_dev(...)
```

can be passed directly into libblur.

It also matches resvg's existing strategy conceptually:

```text
Gaussian requested
       |
       v
large sigma?
       |
       v
multiple box blurs approximating Gaussian
```

This is therefore the least-surprising SIMD replacement for resvg 0.48.1.

---

## 7. Small-sigma behavior

Keep upstream resvg behavior for small Gaussian blurs.

Recommended routing:

```text
sigma < 2
    -> resvg original IIR blur

sigma >= 2 on both axes
    -> libblur gaussian_box_blur
```

The initial implementation should require both axes to be at least 2:

```rust
std_dx >= 2.0 && std_dy >= 2.0
```

This avoids immediately changing behavior for asymmetric edge cases such as:

```xml
<feGaussianBlur stdDeviation="0 10"/>
```

Those can be optimized separately after validating libblur's behavior for effectively one-dimensional blurs.

---

## 8. Lambda threading

The current patch hardcodes:

```rust
ThreadingPolicy::Single
```

That may or may not be correct depending on how the Lambda is parallelized.

Recommended runtime switch:

```text
RESVG_BLUR_THREADS=single
RESVG_BLUR_THREADS=adaptive
```

Use `single` if one Lambda is already rendering several frames concurrently.

For example:

```text
10 frames concurrently
    x
multiple libblur worker threads per frame
```

can oversubscribe the Lambda and make performance worse.

Use `adaptive` if each Lambda is rendering only one frame/render task at a time and has multiple allocated vCPUs.

---

## 9. Keep a runtime fallback

Use:

```text
RESVG_BLUR_BACKEND=libblur
```

for production testing and:

```text
RESVG_BLUR_BACKEND=original
```

for visual/performance comparison.

This is useful because the libblur three-box approximation will not be pixel-identical to upstream resvg's implementation even though both approximate the same Gaussian sigma.

---

## 10. Also fuse consecutive SVG Gaussians

Independent of the resvg patch, optimize SVG filters before rendering.

For the example:

```xml
<feGaussianBlur stdDeviation="18.4729532 18.4729532"/>
<feGaussianBlur stdDeviation="18.4729532 18.4729532"/>
<feGaussianBlur stdDeviation="18.4729532 18.4729532"/>
```

three identical Gaussian blurs can be combined exactly:

```text
sigma_combined = sqrt(
    sigma_1^2 +
    sigma_2^2 +
    sigma_3^2
)
```

So:

```text
18.4729532 * sqrt(3)
    ~= 31.9960935
```

Replace the chain with:

```xml
<feGaussianBlur
    stdDeviation="31.9960935 31.9960935"/>
```

provided:

- the blurs are directly sequential
- there are no intervening filter operations
- the intermediate results are not consumed by another branch
- filter-region clipping does not intentionally alter the intermediate results

This removes two complete filter executions.

---

## Recommended final architecture

```text
SVG
 |
 +-- fuse consecutive Gaussian blurs
 |
 v
resvg 0.48.1
 |
 +-- sigma < 2
 |     |
 |     +-- original resvg IIR
 |
 +-- sigma >= 2 on both axes
       |
       +-- libblur gaussian_box_blur
              |
              +-- ARM NEON
              +-- direct borrow from Pixmap
              +-- one wrapper-owned output allocation
              +-- no Rc deep clone on successful path
              +-- Single or Adaptive threading
```

---

## Optimization priority

Implement changes in this order:

1. **Replace exact `gaussian_blur` with `gaussian_box_blur` for large sigma.**
2. **Move `take()` after the libblur attempt.**
3. **Borrow tiny-skia's pixel buffer directly instead of creating a `src Vec<u8>`.**
4. **Transfer the destination `Vec<u8>` directly into `tiny_skia::Pixmap`.**
5. **Benchmark `ThreadingPolicy::Single` vs `Adaptive` under the actual Lambda concurrency model.**
6. **Fuse sequential SVG Gaussian blurs before resvg.**
7. Only after that, evaluate `fast_gaussian_next` as a more aggressive approximation.

The main issue with the original patch is not that NEON/SIMD is ineffective. It is that SIMD is being used to run an unnecessarily expensive exact Gaussian algorithm, while upstream resvg intentionally chooses a much cheaper approximation for large blur radii.
