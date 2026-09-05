# Diagnosing resvg Render Slowness After Gaussian Blur Ablation

The Gaussian blur is **not** the dominant bottleneck for this asset if removing the blur chain produces nearly the same end-to-end render time.

That changes the optimization strategy substantially.

The next likely bottlenecks are:

1. **Path rasterization / antialiasing**
2. **Offscreen compositing caused by `mix-blend-mode`**
3. **usvg parsing / tree construction / `<use>` expansion**
4. **Clip-path processing**
5. Gaussian blur

This SVG is structurally heavy:

- many complex paths
- many `<use>` references
- many groups
- 21 `mix-blend-mode` uses, primarily `multiply` and `overlay`

The goal of the next experiments is to determine whether the runtime is dominated by:

- pixel-level rendering/compositing
- vector path rasterization
- SVG parsing/tree construction
- clipping
- some combination of these

---

## 1. Ablate all `mix-blend-mode` usage

This should be the next experiment.

Temporarily replace or remove:

```css
mix-blend-mode: multiply
mix-blend-mode: overlay
```

so all affected elements render with normal compositing.

For example:

```svg
<use
    style="mix-blend-mode: multiply"
    ...
/>
```

becomes:

```svg
<use
    ...
/>
```

Do this for all blend-mode uses in the SVG.

### Why this matters

In resvg, non-normal blending can require an isolated offscreen group.

Conceptually:

```text
calculate group bounds
        |
        v
allocate temporary pixmap
        |
        v
render group contents
        |
        v
blend temporary pixmap into parent
```

This means a single `mix-blend-mode` declaration can cause:

- another large allocation
- another complete render into an intermediate buffer
- another pass over pixels when compositing back
- additional memory bandwidth

With many large blend groups, this can dominate rendering even when Gaussian blur does not.

### Interpret the result

If removing blend modes makes rendering dramatically faster:

```text
blend/isolation/compositing is the main bottleneck
```

If performance barely changes:

```text
move on to path rasterization and parsing
```

---

## 2. Ablate clip paths

Temporarily remove clip paths and compare render time.

For example, turn:

```svg
<g clip-path="url(#clipPath0)">
    ...
</g>
```

into:

```svg
<g>
    ...
</g>
```

Do this only for benchmarking.

### Interpret the result

A substantial improvement means clipping is contributing meaningful overhead.

A small or negligible difference means clipping is not a priority.

---

## 3. Separate usvg parse time from resvg render time

Do not benchmark only the complete:

```text
SVG bytes -> PNG
```

pipeline.

Measure parsing/tree construction and rasterization separately.

Example:

```rust
let t0 = Instant::now();

let tree = usvg::Tree::from_data(&svg_data, &options)?;

let t1 = Instant::now();

resvg::render(&tree, transform, &mut pixmap.as_mut());

let t2 = Instant::now();

println!("parse:  {:?}", t1 - t0);
println!("render: {:?}", t2 - t1);
println!("total:  {:?}", t2 - t0);
```

The exact API may differ depending on your surrounding code, but the important thing is to measure:

```text
usvg parse/tree construction
vs.
resvg rendering
```

separately.

### Why this matters

`usvg` performs substantial normalization work, including:

- resolving `<use>`
- resolving referenced definitions
- parsing and normalizing paths
- resolving styles
- normalizing transforms and paint
- constructing its internal tree

If your measurements look like:

```text
parse:   40 ms
render:  25 ms
total:   65 ms
```

then optimizing the resvg rasterizer alone cannot produce a huge end-to-end improvement.

---

## 4. Perform a resolution-scaling test

Render the same SVG at two resolutions.

For example:

```text
full:
1961 x 1170

half:
980 x 585
```

The half-resolution image contains roughly one quarter as many pixels.

### Interpretation

#### Runtime drops roughly 3-4x

Likely dominated by pixel work:

- rasterization
- antialiasing
- blending
- offscreen compositing
- memory bandwidth

#### Runtime barely changes

Likely dominated by non-pixel work:

- XML/SVG parsing
- usvg tree construction
- path parsing/normalization
- resolving `<use>`
- fixed per-element overhead

#### Runtime drops around 2x

Likely a mixture of:

- vector/tree processing
- pixel-level rasterization/compositing

This is one of the most useful diagnostic tests because it quickly distinguishes CPU work that scales with pixel count from work that mostly scales with SVG complexity.

---

## 5. Test parsed-tree reuse

If multiple frames reuse identical or mostly identical SVG structure, determine whether you are reparsing the SVG every time.

Benchmark:

```text
A:
parse SVG
render
parse SVG
render
parse SVG
render
```

against:

```text
B:
parse SVG once
render
render
render
```

If parsed-tree reuse substantially improves throughput, then `usvg` parsing/tree construction is significant.

This is especially important if your animation pipeline renders many related frames.

---

## 6. Measure path-rasterization sensitivity

This asset contains a large amount of path data.

If:

- blur removal does not help
- blend removal does not help much
- half resolution improves substantially

then complex path rasterization is a strong candidate.

Useful experiments:

### A. Remove strokes

Temporarily change complex stroked paths such as:

```svg
stroke="#000000"
stroke-width="..."
```

to:

```svg
stroke="none"
```

and benchmark.

Large rounded strokes with joins/caps can be more expensive than fills.

### B. Render only subsets of the scene

Temporarily remove major top-level sprite groups and identify which group consumes most of the render time.

For example:

```text
whole asset
    |
    +-- body
    +-- head
    +-- wings
    +-- glow
    +-- hair
    +-- accessories
```

Benchmark each major section or progressively remove them.

This can reveal one pathological subtree.

### C. Compare path count vs. path complexity

Two SVGs with the same number of paths can have very different cost.

Important variables include:

- number of curve segments
- path length
- stroke complexity
- self-intersections
- fill rules
- transform scaling
- clipping
- blend/isolation boundaries

---

## 7. Profile allocations and memory traffic

If blend modes or isolated groups are important, the workload may be memory-bandwidth bound rather than compute-bound.

Track:

- number of temporary pixmaps
- width/height of each temporary pixmap
- total allocated pixel bytes
- number of full-surface copies
- peak resident memory
- time spent allocating/freeing buffers

A useful log format would be:

```text
group isolate:
  size: 1800 x 950
  bytes: 6.52 MiB
  blend: multiply

group isolate:
  size: 1450 x 800
  bytes: 4.43 MiB
  blend: overlay
```

This can quickly expose a few giant groups causing most of the cost.

---

## 8. Keep Gaussian blur out of the critical path for now

The blur ablation showed that Gaussian blur is not the primary bottleneck for this asset.

Therefore, do **not** prioritize:

- more libblur tuning
- exact vs. fixed Gaussian
- further Gaussian SIMD work
- custom blur threading
- more aggressive Gaussian approximation

until the larger bottleneck is identified.

The existing blur optimization work may still help some assets, but it will not materially improve this particular asset if the ablation result is representative.

---

## 9. Recommended ablation sequence

Run these experiments in this order:

```text
Baseline
   |
   +-- A. Remove Gaussian blur
   |      already done: approximately no improvement
   |
   +-- B. Remove all mix-blend-mode
   |
   +-- C. Remove clip paths
   |
   +-- D. Separate usvg parse vs. resvg render
   |
   +-- E. Render at half resolution
   |
   +-- F. Remove strokes
   |
   +-- G. Remove major sprite groups one at a time
```

Record for each run:

```text
parse time
render time
total time
peak RSS
output dimensions
Lambda memory setting
Lambda architecture
number of concurrent renders
```

Do multiple runs and compare medians rather than relying on one invocation.

---

## 10. Most likely diagnosis after the blur ablation

For this asset, the current working hypothesis should be:

```text
1. complex path rasterization / antialiasing
2. isolated layers caused by multiply/overlay
3. usvg tree construction and <use> resolution
4. clipping
5. Gaussian blur
```

The exact order of #1 and #2 should be determined by the blend-mode ablation.

---

## 11. Implication for ThorVG testing

The blur ablation actually makes a ThorVG benchmark **more relevant**, not less.

The question is no longer:

```text
Which renderer has the fastest Gaussian blur?
```

It becomes:

```text
Which renderer has the fastest overall:
- path rasterizer
- antialiaser
- compositor
- blend implementation
- offscreen-layer handling
on ARM?
```

Benchmark at least:

```text
resvg 0.48.1
vs.
patched resvg
vs.
ThorVG software renderer
```

using the exact same:

- SVG
- output resolution
- Lambda architecture
- memory/vCPU allocation
- concurrency
- warm/cold state

Measure:

```text
parse/load
render
total
peak RSS
visual difference
```

If ThorVG is faster because of general rasterization/compositing rather than Gaussian blur, that could make it valuable across a much larger portion of the AQW asset corpus.

---

# Immediate Next Test

The single highest-value next experiment is:

```text
remove all mix-blend-mode declarations
```

and compare:

```text
original render time
vs.
normal-blend render time
```

After that, run the **half-resolution test** and **parse-vs-render timing**.

Those three measurements should tell you whether the real bottleneck is:

```text
offscreen compositing
path rasterization / pixel processing
or
usvg parsing/tree construction
```

---

# 2026-09-04 ablation results (Godlow LaeDWearGoldDragon ground)

Ran the built component SVG (4096x2478, ~2.3M px tight page) through the
in-process resvg 0.48.1 via `aqw-component-raster bench-svg` on an M1 with
`RESVG_BLUR_BACKEND=original`. Median-ish single runs:

| test | render time | vs baseline | verdict |
|---|---|---|---|
| baseline           | 179.3 s | 1.00x | - |
| no blur (sigma->0) | ~160-240 s | ~1.0x | blur NOT bottleneck |
| no blend modes     | 177.8 s | ~1.0x | mix-blend-mode NOT bottleneck |
| no clip-path       | 167.4 s | 0.93x | clipping minor |
| no stroke          | 157.5 s | 0.88x | strokes minor |
| half res 2048x1239 | 11.3 s  | 0.063x | pixel work dominates |
| quarter 1024x620   | 1.0 s   | 0.006x | pixel work dominates |

Pixel area scaling: 16x fewer pixels -> ~180x faster (super-linear, ~px^1.7).
This is a **rasterization/compositing/antialiasing/memory-bandwidth**
bottleneck, not feature-level. Neither blurred chains, blend isolation,
clipping, nor strokes are significant for this asset.

Implication: the lever is **rendering the ground at a lower raster scale**
(plan option A): 2048 ~ 16x faster, 1024 ~ 180x faster here, and the ground
is downsampled to 2048 anyway.
