# Raster performance follow-up — September 6, 2026

I would keep patched resvg as the production baseline and improve its filter executor. The strongest next candidates are the small-sigma IIR traversal, conservative filter working regions, and specialized color-matrix execution. Copy elimination is worthwhile, especially for memory usage, but the profiling does not support treating explicit copying as the largest CPU cost. Generic SVG minification is unlikely to help these difficult components materially.

No repository or sibling-package files were changed. Source inspection began at `9209cd4`; the user's subsequent bounds/cache changes remain untouched. The local instrumented renderer and experimental IIR patch exist only in this directory. AWS tests invoked the existing raster worker with cache-disabled manifests under a fresh temporary job prefix. They did not deploy code, change configuration, submit a full workflow, or notify Discord.

## New AWS evidence

Nine isolated invocations used `aqw-char-dev-componentraster-rust`, ARM64, 3,008 MiB, `RESVG_BLUR_THREADS=single`. The worker revision checked before each invocation remained `4a07678a-ca1a-425e-976c-f4cf1e3e6ecb`; code digest `c8c76e67bfdd6c715e4798775107fc0f4802a498fd19ce831bb435f6694634b7`. Its configuration was last modified at `2026-09-06T05:25:17Z`.

The inputs were one saved Annie ground state, task 30 from job `8f82d491-e693-473f-8c29-fd4dc0673264`, and the expensive Dalvi cape state `6d9739d357d8c676e26839f1ebfdcd0dc8b0bd547501f5911a55c542e9ecfae4` from `5a400e21-8def-4fcc-92a4-675653d6ad20`. Both retained their 4096 raster / 2048 output settings and original placement.

| Diagnostic variant | Annie dragon raster | Dalvi cape raster |
| --- | ---: | ---: |
| Original | **43.89 s; repeat 42.08 s** | **13.36 s** |
| Gaussian sigma set to zero | 21.84 s | Not tested |
| Remove authored mix-blend-mode declarations | 41.94 s | Not tested |
| Remove generated tint rules | 29.64 s | 9.81 s |
| Authored filter regions 300% → 200% | **25.89 s** | **9.56 s** |

These are AWS raster times from the returned task records, not M1 extrapolations. They isolate one state and do not establish full-Map latency under concurrency. Except for the repeated dragon baseline, each variant was run once. Disabling tints also changes group/filter isolation, so its time difference is not a pure measurement of matrix arithmetic. Ablation results are not additive and removing effects is not an optimization to ship.

The repeated dragon baseline produced identical PNG hashes. The 200% region test preserved dimensions and integer placement, but was not pixel-exact:

| Region variant | Pixels compared | Changed pixels | Pixels differing >1 | Maximum channel difference |
| --- | ---: | ---: | ---: | ---: |
| Dragon | 2,445,236 | 603 | 6 | 7 / 255 |
| Cape | 1,821,043 | 350 | 301 | **127 / 255** |

The cape result shows why tiny average error is insufficient: a small affected area can still have substantial clipping/alpha differences. Compute tighter regions from actual inputs and effect support; do not hardcode 200% globally. These tests encoded component PNGs, not complete WebP/AVIF animations, so they establish no final-animation size reduction.

## The small-sigma path deserves priority

The successful large-sigma libblur path is already patched. It borrows input pixels and transfers its destination buffer into the resulting pixmap, avoiding the old wrapper copies. The remaining threshold is based on **transformed pixel-space sigma**, not just `stdDeviation` in the SVG. In the current code, both axes must be at least 2 to use this patched path; a large sigma on one axis alone does not qualify.

I built a separate diagnostic copy of the current resvg source and rendered the assembled 4096×2699 dragon SVG. This is **local M1 profiling, not AWS timing**:

| Work in the local diagnostic run | Calls | Pixel visits across calls | Time |
| --- | ---: | ---: | ---: |
| Patched libblur | 69 | 751,895,673 | 6.48 s |
| Small-sigma IIR | **12** | **59,191,683** | **21.58 s** |
| Color matrices | 139 | 1,232,775,275 | 7.81 s |
| Filter composites | 26 | 240,500,020 | 3.20 s |
| Parse SVG | — | — | **0.011 s** |
| Whole resvg render | — | — | 50.77 s |

Calls exceed authored definition counts because referenced subtrees execute more than once. Despite operating on much fewer pixels, the small-sigma calls dominate local blur time. Some transformed sigmas are around 1.42 even at 4096, so this issue is not limited to 2048 raster requests.

The AWS all-blur ablation corroborates that blur remains important overall, but **does not separately measure AWS IIR versus libblur time**. The local per-path ratios must not be applied to Lambda.

### A smaller first patch than replacing the algorithm

The existing IIR implementation uses a full-image `f64` scratch plane and four passes per direction for each RGBA channel. Its vertical loop processes an entire column before moving to the next column, repeatedly accessing memory at a full-image-row stride.

I changed only the vertical traversal in the temporary copy: run each recurrence step across contiguous rows, updating independent columns together. Each column retains the same down/up recurrence and step order. This improves memory locality and may permit vectorization across columns without replacing the blur approximation, coefficients, precision, or number of passes.

The experimental output matches the original **exactly in decoded RGBA** for the full 4096×2699 dragon and all **18 synthetic cases**, including random alpha, one-pixel-wide/tall images, zero sigma on one axis, and several sigmas below 2. This is good evidence for the patch, not an exhaustive platform proof.

The first modified local run took 32.52 s, with IIR time 11.25 s. However, untouched libblur work also fell from 6.48 to 4.11 s between the runs, so **50.77 → 32.52 s cannot be attributed entirely to the patch**. There was substantial local timing variation. A second pair measured 64.84 → 34.24 s overall, with IIR 28.54 → 11.90 s, but unchanged libblur also moved 8.29 → 4.33 s. Thus neither pair cleanly isolates a whole-render speedup. The IIR-to-libblur time ratio improved in both pairs (approximately 3.3–3.4 to 2.7–2.8), which supports further testing but is not a controlled CPU-normalized benchmark. A controlled AWS build of this patch remains the next performance test. All four runs are in `paired-local-results.json`. This experimental patch has not been applied to the repository or deployed.

Files: `iir-row-traversal.patch`, `iir-synthetic-checks.json`, `local-profile-summary.json`, `blur-profile.json`, `iir-row-summary.json`.

If traversal optimization is insufficient, compare a SIMD separable small-kernel Gaussian against IIR. That is an algorithm change requiring explicit quality validation. Do not merely route all small sigmas to the existing three-box path: tiny/anisotropic/zero-axis cases and edge behavior need separate handling.

## The copying concern is real, but distinguish traffic from time

Source locations in the unmodified engine snapshot:

- `filter/mod.rs::get_input`: `SourceGraphic` clones its pixmap on each access; `SourceAlpha` clones and clears RGB.
- `Image::take`: failed `Rc::try_unwrap` triggers a complete pixmap clone.
- `apply_inner`: retains all primitive results until the filter ends. A later primitive borrows a prior result via another `Rc`, so mutation often requires copying even when that prior result has no future consumers.
- `apply_color_matrix`: obtains a mutable pixmap, demultiplies, applies the generic matrix, then multiplies alpha again.
- `apply_composite`: creates another surface and draws both inputs into it.
- `apply_to_canvas`: clears the original surface and draws the final filter result back into it.
- `render_group`: allocates an isolated surface, renders/filters it, and composites it into its parent.

The local dragon run counted **140 SourceGraphic copies totaling 5.05 GB**, plus **64 copy-on-write fallbacks totaling 2.16 GB**. Together those explicitly timed clones took **1.13 s** of the 50.77 s local render. This excludes other draws, clears, allocations, and memory effects; it is not total memory traffic.

There were also 130 isolated group allocations totaling **4.32 GB cumulatively**. The largest individual group surface was **7565×6096**, about 184 MB, despite a final raster page of 4096×2699. These figures are sums over execution, not peak resident memory. AWS reported roughly 1.5 GB high-water memory, although a reused environment's high-water value is not an independent per-variant measurement.

Recommended resvg changes:

1. Resolve primitive inputs into a dependency graph and count remaining consumers. Drop/move a result after its last use instead of keeping every result live. Handle repeated names, default inputs, branching, and two-input primitives correctly.
2. Share immutable SourceGraphic/SourceAlpha representations within a filter and clone only when a live consumer requires independent mutation.
3. Mutate an exclusively owned input for suitable pointwise operations; transfer final-buffer ownership when equivalent to clear-and-draw.
4. Introduce bounded scratch-buffer reuse after ownership is correct. Clear reused regions correctly and cap retained memory.

These can reduce peak memory and redundant work while preserving pixel math. The current evidence does **not** justify promising a huge CPU speedup merely from removing `.clone()` calls.

## Color matrices and filter regions: high-value resvg targets

The AWS tint ablation and the local 1.23 billion matrix pixel visits show that this is substantive work. Most generated matrices are much simpler than a general 4×5 matrix.

A constant RGB tint with alpha passthrough needs the input alpha, but does not need to demultiply and multiply the original RGB. A dedicated kernel can produce the tinted premultiplied result with matching clamping and rounding. Other common cases include identity and diagonal multiplier/offset transforms. Classify the matrix once and dispatch to a specialized kernel; retain the generic path for arbitrary matrices.

Fusing demultiply → matrix → premultiply into one pixel loop saves surface passes. Preserve the existing intermediate rounding if claiming byte equality. Precomputed lookup tables are another exact candidate for frequently reused diagonal transforms, but their setup/cache cost must be measured.

A pointwise matrix that maps transparent black to transparent black does not expand the input's painted extent. Limit its work to the actual input effect bounds, including nested effects and strokes. Alpha offsets or other operations that create visible output from transparent input need the broader region. Keep filter clipping and isolation semantics even when simplifying arithmetic.

For blur and composite chains, work backwards from the output region needed by the caller to determine required input rectangles and blur halos. Preserve coordinate systems and integer raster alignment. Finite box kernels have a concrete support radius; recursive IIR has different boundary/support considerations. A conservative implementation can fall back for chains whose required extent is not yet proven.

This combines the two most relevant findings: huge transparent working surfaces and a costly operation repeated over them. It targets first-render CPU time and does not inherently require bigger delivered images.

## Can the SVGs contain fewer commands?

Yes, but distinguish fewer XML tokens from fewer rendering operations. The assembled dragon has 82 path definitions, approximately 7,906 path commands, 156 use nodes, and 32 filter definitions. Parsing took approximately 11 ms in the local diagnostic run. Whitespace removal, shorter IDs, or generic path serialization changes will not remove seconds of filter processing.

Useful semantic optimizations to consider:

- Remove unreachable definitions and filter primitives after resolving references. The current usvg filter parser explicitly has a TODO to remove primitive results that are never used. This is a corpus opportunity, not a measured large win for this dragon's mostly live filter chains.
- Eliminate provably redundant operations such as zero offsets, with reference rewrites and preserved subregion/color-space behavior. A zero offset already returns its input in resvg, but unnecessary retained references can still force later copying.
- Fuse consecutive pointwise operations into one traversal while preserving every stage's clamp/rounding and only when intermediate results have no other consumers.
- Reuse identical rasterized geometry/alpha masks at the same transform, scale, and pixel phase. Exact translated/unchanged subtrees are a safer starting point than rotating cached bitmaps.
- Reuse stable filtered subtrees across animation frames. Include strokes, color, blend/isolation, clipping, and filter context in their identity; do not assume matching paths imply matching rendered pixels.

Two tempting rewrites are not generally lossless:

- Multiplying consecutive color matrices changes results when an intermediate stage clamps. For example, doubling a normalized value of 0.8 clamps to 1, then halving produces 0.5; a combined identity matrix yields 0.8. Preserve intermediate operations even if they execute within one loop. [Filter primitive clamping rules](https://www.w3.org/TR/filter-effects-1/#FilterPrimitiveOverview)
- This dragon's authored glow chains often contain three consecutive Gaussian blur operations. Replacing them with one blur at the square-root-of-summed-variances sigma is an ideal continuous-Gaussian identity, but the actual renderer uses discrete approximations, finite regions, and 8-bit intermediate rounding. Treat such collapse as a separate approximate-quality experiment.

Likewise, removing duplicate-looking draws can change antialiased edges and alpha accumulation; flattening groups can change blending/isolation. These are not safe global “optimize SVG” switches.

A more ambitious specialized glow implementation could blur only the alpha mask and recolor afterward when the chain has a constant-color glow. It potentially reduces channel work and intermediate storage, but the existing RGBA quantization/alpha-boost behavior can produce differences. It belongs after the exact traversal and matrix-kernel improvements, with its own visual/alpha comparisons.

## ThorVG: keep it a restricted candidate

The current tree already contains the minimal `feColorMatrix type="matrix"` patch from `992def9`. That supports a limited use case; it does not establish general filter compatibility.

The actual vendored loader recognizes Gaussian blur and the added matrix primitive. The matrix parser reads type/values, not the general `in`/`result` graph. The builder makes one pass adding **all matrices**, then a second adding **all Gaussian blurs**. This differs from the document's statement that mixed primitives execute in document order. Arbitrary graph input/output names, composite operations, and color-space semantics are not thereby implemented.

The dragon uses chains like:

```text
SourceGraphic → zero offset → constant color → blur → blur → blur
              → alpha boost → composite original artwork over the glow
```

A renderer that skips the final composite or moves alpha boost before blur is not executing the same effect. Missing work can also make an engine appear fast. A whole-component tint pass after ThorVG cannot repair arbitrary internal color/filter/blend ordering.

The wrapper currently calls `tvg_engine_init(0)`, making its own comparison path single-threaded. Given the user's observation that comparable single-core speed was similar to resvg, there is not yet a strong reason to invest in implementing a broad SVG filter engine inside ThorVG.

A viable limited approach is to classify the **reachable** SVG feature set, use ThorVG only for a validated subset, and fall back to resvg before rendering unsupported cases. Benchmark that subset on the actual Lambda CPU with matched threads and pixels. If expensive dragon/cape effects always require the fallback, the benefit to the slowest renders may be small.

## Suggested order

1. Validate the scratch IIR traversal patch in a separate AWS benchmark build. It preserves the algorithm and passed the local pixel comparisons.
2. Add conservative working-region calculations and specialized tint/diagonal matrix kernels; coordinate with the user's ongoing component-bounds work.
3. Add primitive-result lifetime analysis and bounded scratch reuse. Measure both CPU and peak memory.
4. Evaluate another small-sigma blur algorithm only if needed after traversal changes, with explicit quality thresholds.
5. Pursue reusable filtered subtrees/alpha masks for a larger architectural speedup.
6. Revisit ThorVG only with a capability gate and demonstrated equal-feature AWS advantage.

Use original RGBA output for comparison, retaining 4096→2048 sampling, full animation timing, alpha, and all visual effects. Pure execution/ownership changes should preserve pixels and hence need not increase encoded bytes. Approximate region/blur changes need actual final-animation size and quality measurements; a smaller SVG or component PNG alone does not prove a smaller Discord attachment.

Evidence, scripts, original/mutated assets, local copies, and the unapplied patch are all in `/private/tmp/aqw-raster-deep-20260906`. AWS scratch object prefix is recorded in `aws-prefix.txt` and is under the existing temporary `jobs/` lifecycle. Production configuration and shared caches were untouched.
