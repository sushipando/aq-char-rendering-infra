# Component-Raster Speed-Up Ideas

Status: research notes, not yet implemented. All timings are local macOS (M1)
against resvg 0.48.1 unless stated; the deployed 34s wing tasks would scale by
the (measured) ~4.5x Lambda-x86-vs-M1 factor.

## The two dominant costs (measured)

### 1. feGaussianBlur is the big one (blur-bound items)

Slowest EmpressWings frame (raster page ~3900x1720), single ablations:

| ablation | time left | that feature's cost |
|---|---|---|
| full (authored) | 5.81s | - |
| blur std -> 0 | 1.55s | **4.26s (73%)** |
| gradients stripped (flat fill) | 5.45s | 0.36s (6%) |
| shapes/color-matrix/composite base | ~1.2s | ~21% |

- The wing feather is `Offset -> ColorMatrix -> GaussianBlur(std 3.16, both
  axes) -> ColorMatrix -> Composite` applied to each wing panel.
- resvg implements GaussianBlur as **scalar IIR/box blur** (`iir_blur.rs`,
  `box_blur.rs`) — plain per-pixel loops, **no SIMD**.
- Scaling: cost grows ~linearly with raster area (2048: 3.6s, 1024: 1.5s,
  512: 0.4s).
- Deployed tasks at 4096 raster run 15-35s (Lambda x86 ~2.7-4.5x slower than
  M1 on this scalar workload).

### 2. Filter region over-estimation (the lever that works TODAY)

The authored feather filters use `x="-100%" y="-100%" width="300%"
height="300%"` — a **9x bbox region** — and every pixel in that region goes
through the full blur pipeline. Trimming the filter region to roughly the
panel bbox puts most of the region outside the blur input:

| filter region | slowest frame |
|---|---|
| -100/-100/300/300 (authored) | 6.50s |
| -50/-50/200/200 | **2.34s (2.8x)** |
| -60/-60/220/220 | 2.35s |
| -25/-25/150/150 | 2.50s |

So an SVG-level fix (shrink the feather filter region to ~2x bbox or add a
natural bbox) alone gives ~2.8x on the wing frames with **zero engine work**.
That's the highest-value, lowest-risk item.

## Speed-up ladder

### A. Config-only (no code)
- Run at raster 2048 instead of 4096 (~4x on blur-bound tasks); matches dev
  tuning. Smoke tests pass `raster=output*2` explicitly today.

### B. SVG/output changes (no engine work, deploy today)
- **Trim feGaussianBlur filter regions** to the artwork bbox (+ margin) to
  shrink the blurred pixel count (2.8x measured). This is safe because the
  final frame canvas still clips at its edges like the composed pipeline.
- Set `color-interpolation-filters="sRGB"` already; optionally raise the blur
  `stdDeviation`-equivalent quality only if it doesn't equal authored.

### C. Engine-level: enable SIMD properly
- **We are silently NOT using tiny-skia AVX today.** tiny-skia 0.12 gates
  `__m256` on `cfg(all(feature="simd", target_feature="avx"))`. We enable the
  `simd` feature (via resvg default), but our Dockerfile pins
  `RUSTFLAGS="-C target-cpu=x86-64-v2"` which **excludes AVX** — so the
  build falls back to `f32x4` pairs, i.e. the scalar/composite path, not
  `__m256`.
- **Fix**: build with `-C target-cpu=x86-64-v3` (adds AVX/AVX2/FMA/BMI) — safe
  for Lambda x86 Xeons (Skylake+ / Sapphire Rapids all have AVX2). This is a
  one-line Dockerfile change. (Note: f32x8 is used for fills/blends; the blur
  itself is still scalar, so this helps the 21% "shapes/color-matrix" chunk,
  not the blur directly.)
- Optionally also `-C target-cpu=x86-64-v4` for AVX-512 on Sapphire Rapids;
  not all Lambda x86 supports it yet, but worth an A/B (would need runtime
  feature detection or reserving to a known instance type).

### D. Algorithmic: blur-cache / panel-blit (the "execution tree" idea)
Core observation (verified): consecutive wing frames share **100% of path
geometry** and differ only in panel transform matrices (~0.2% rotation /
translation per frame). So per-frame work is redundant.

Because an isotropic Gaussian commutes with rotation/translation to the
level that matters visually, a reusable cache keyed by
`(panel geometry hash, filter settings, blur std)` lets us:
1. Rasterize + blur each static panel **once**.
2. Per frame, transform-composite the cached blurred panel (a blit, not a
   re-blur).

That should reduce wing states from ~35s toward the sub-second floor of a
single rasterization. This is the general pattern for "static expensive core
+ small animated overlay" (blink flicker, cape shimmer, weapon glow), very
common in AQW.

Implementation sketch (in the Rust raster worker, since we already consume
the FFDec SVG export as the scene graph):
- Parse the component SVG into a tree of `<use>` nodes; identify subtrees
  whose paths/geometry are invariant across frames (content hash).
- For each static subtree, render to a `tiny_skia::Pixmap` + apply filters;
  cache by hash (S3 content-addressed, like `vector-states/`).
- Compose per frame by applying the *transform on the `<use>`* onto the
  cached pixmap and alpha-blending.

Sub-problems to verify:
- Exactness: Gaussian vs rotation commute only approximately; acceptable
  error budget to define (visually <1px).
- Filter output depends on filter region + bbox; cache key must include the
  panel bbox, not just geometry.
- Memory: 34s tasks are ~12-34MB; caching a handful of panels per frame is
  cheap.

### E. Engine: replace resvg blur with a SIMD blur
- The `feGaussianBlur` implementation is the hotspot; a SSE2/AVX2 separable
  blur (or `Libwebp`-style) could cut its cost by 3-4x. That is an engine
  fork/PR (resvg upstream) — defer until the region-trim + v3 flags are
  proven, because both are easier and don't fork.

## Profiling recommendation
Current profiling is local + deployed timings + manual ablations. For the
real 35s tasks, add a per-primitive timing build (usvg parse / gradient /
blur / colormatrix / composite) and run it **on Lambda-class x86 at the real
page size**, so we attribute precisely instead of by M1 extrapolation.

## Not a win
- Replacing the Pillow-exact raster stage compiler flags alone won't help the
  blur (scalar).
- `fast_image_resize` FFIR was already ruled out for downsampling (bbox flips);
  it applies only to the downsample step, which is 1.2% of stage time.