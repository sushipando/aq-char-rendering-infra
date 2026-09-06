# Faster, smaller 2048px character renders: investigation findings

Investigated September 6, 2026, America/Los_Angeles. Repository HEAD: `aef4946`, September 5 at 18:57 PDT, plus the working tree changes already present when this investigation started. AWS observations are from `aqw-char-dev` in `us-west-2`.

**There are substantial CPU-only opportunities without reducing output resolution.** The strongest immediate size improvement is to merge adjacent identical WebP frames by extending their duration. The strongest raster opportunities are to reuse the visible bounds already computed, eliminate provably invisible states from framing, reduce filter intermediate allocations, and cache expensive components with their actual colors. Animated AVIF is a credible route to small, crisp files, but its encoder must be integrated around temporal compression and available CPU; simply adding a serial conversion after today's WebP pipeline would increase latency.

No source code, dependencies, infrastructure, or sibling packages were modified. No renders were submitted, messages sent, deployments performed, or quota requests opened. The only repository addition is this report. Investigation scripts and new artifacts are under `/private/tmp`; existing assets and benchmarks were read in place.

## M1 measurements are screening evidence, not AWS forecasts

**No M1-to-Lambda conversion factor is assumed in this report.** The local M1 Mac and AWS Lambda arm64/Graviton2 are different CPU platforms. ARM64 is an instruction-set family, not a promise of equivalent core speed, memory bandwidth, scheduling, or encoder behavior. The Mac and Lambda may also differ in compiler flags, renderer/library revision, thread count, cold starts, S3 I/O, and the amount of CPU actually allocated. `-j 2` on the Mac limits encoder threads; it does not emulate Lambda's approximately 1.7 vCPU allocation at 3,008 MiB, its CPU entitlement, or its hardware. [AWS Lambda architecture](https://docs.aws.amazon.com/lambda/latest/dg/foundation-arch.html)

Use the evidence in three distinct ways:

| Evidence | What it establishes | What it does not establish |
| --- | --- | --- |
| Current Step Functions and CloudWatch data | Actual AWS wall time and instrumented phase costs for those executions | A controlled comparison between different jobs/builds |
| Local exact remux and pixel comparisons | Those bytes can be removed without changing the displayed image/timing | That Lambda will perform the operation in the local 28 ms |
| Local raster/codec benchmarks | Candidate algorithms, quality/size tradeoffs, and suspicious dispatch behavior | AWS stage latency, billed cost, or a production speedup ratio |

Before choosing a production profile, replay the **same frozen fixtures** on an isolated Lambda benchmark using the production ARM64 container/tool versions, explicit memory and thread settings, and the same raster/output sizes. Measure cold and warm runs separately, along with billed duration, memory high water, I/O, and output quality/bytes. None of the local speedup ratios below should be multiplied into today's AWS timings.

## What the current AWS renders actually spend time on

These are individual observed executions, **not controlled p50/p95 benchmarks**. Different appearances, cache settings, Lambda warmth, and deployment revisions prevent treating them as direct A/B comparisons. Durations below come from Step Functions outer state/Map entry and exit timestamps; concurrent Lambda durations must not be summed to estimate wall time. All rows use a 4096px raster and a 2048px output, Q85/method 4 WebP, and inline raster fan-out. Dimensions preserve aspect ratio; 2048 means the longest side, not necessarily a square.

| Character / job prefix | Output / frames | Total workflow | Export Map | Raster Map | Compose Map | Finalize | Output bytes |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Alina `a28796e8` | 1206×2048 / 120 | 10.43 s | 0.30 s | 4.74 s | 2.86 s | 0.83 s | 16,586,468 |
| Queen Annie 012 `1aa28d78` | 2048×1350 / 120 | 115.77 s | 0.35 s | **106.19 s** | 5.60 s | 1.32 s | 29,128,658 |
| Dalvi `5a400e21` | 1785×2048 / 120 | 56.89 s | 1.84 s | **41.41 s** | 6.67 s | 2.21 s | 64,887,230 |
| Akine `6c7c0336` | 2048×1426 / 120 | 56.52 s | **26.09 s** | 19.08 s | 5.29 s | 1.94 s | 41,214,882 |
| Older_flame `1e92e66b` | 2048×1294 / 120 | 36.64 s | 9.25 s | 17.64 s | 4.92 s | 1.57 s | 38,882,028 |

Prepare, bounds planning, component collection, and state transitions account for the remaining wall time. Alina and Annie had cached vector exports; Annie explicitly disabled component and final-result caches. Dalvi's vector exports were cache hits. Akine's six exports were misses despite enabled caches. Older_flame disabled every compute cache. None of these outputs came from the completed-render shortcut.

CloudWatch profiles sharpen the diagnosis:

- **Annie:** 98 component tasks; 49 ground tasks consume **2,776.89 aggregate raster seconds**, essentially all the job's 2,777.81 raster seconds. Ground median raster time is 53.04 s and maximum 74.06 s. Downsampling across *all* components takes only 8.27 aggregate seconds. Fixing PNG downloads or the final mux cannot remove this bottleneck.
- **Dalvi:** 418 tasks; the 38 cape tasks account for **662.48 of 888.54 aggregate raster seconds**. The slowest cape raster takes 26.84 s. This is both a renderer problem and a scheduling opportunity.
- **Akine:** 49 ground tasks account for 425.45 of 491.59 aggregate raster seconds. Its slowest export spends 22.89 s in the reported FFDec phase and 2.76 s in metadata work. Other exports spend roughly 3–4 s in metadata work, so a vector-cache miss still matters.
- **Composition:** Annie's workers spend 94.89 aggregate seconds encoding versus 1.69 compositing; Dalvi spends 165.93 encoding versus 2.97 compositing. Existing SIMD composition is already small relative to encoding. Further blend-loop tuning is a low priority for these cases.

The older Annie lossless job `8f82d491` produced **110,129,016 bytes** in 107.06 s. Its raster Map took 92.26 s. This is the source used by the existing local AVIF/WebP benchmark. Its different raster timing is not evidence that lossless encoding speeds up rasterization.

## 1. Merge adjacent identical frames: a verified, pixel-exact size win

The September 5 recipe deduplication avoids recomposing and re-encoding repeated frames. However, [finalize.rs](../services/pipeline-rust/src/finalize.rs) downloads each unique WebP once and then passes **every logical frame** to `webpmux` again. Reusing an S3 key does not reuse its compressed payload inside the animation.

I downloaded the current AWS Alina result and repacked only adjacent identical encoded frames. No pixels were re-encoded:

| Measurement | Existing v20 output | Temporary repacked output |
| --- | ---: | ---: |
| Bytes | 16,586,468 | **4,173,156** |
| Physical frames | 120 | **30** |
| Canvas | 1206×2048 | 1206×2048 |
| Animation duration | 5,000 ms | 5,000 ms |
| Decoded RGBA at every original time interval | Reference | **Exact match** |

This saves **74.84%** of the file, retains all motion and timing, and takes approximately **28 ms** for the local container edit, excluding decode verification. It is not a frame-rate reduction. The existing composition deduplication has already captured most upstream compute savings, so this change primarily reduces final mux work, bytes published, and delivery time.

Implementation boundary for later work: create an encoded run schedule after validating all logical frames; combine adjacent records only when payload identity, placement, dimensions, blend/disposal behavior, and other relevant semantics match. Preserve the total duration and split runs if they exceed the WebP 24-bit duration field. Keep logical-frame accounting separate from physical container frames. The current `validate_webp` checks the old physical count/duration array and would need to validate the new run schedule. Nonadjacent repeats cannot be removed by merely adding their duration elsewhere.

**This does not solve Annie:** her existing 120-frame sample has 94 unique decoded frames but no adjacent equal runs. For that case, compression across moving frames matters.

Evidence and runnable scratch reproduction:

```text
/private/tmp/aqw-codec-audit-20260906/verify_adjacent_merge.py
/private/tmp/aqw-codec-audit-20260906/alina-v20-adjacent-merge.json
/private/tmp/aqw-codec-audit-20260906/alina-v20-adjacent-merge.webp
```

### Implementation addendum: adjacent-run merging

Implemented after the read-only audit, on September 6. The original findings
above describe the pre-change baseline. No deployment or AWS job submission was
performed for this implementation.

The Rust finalizer now validates every logical frame record, sorts by logical
frame number across batch boundaries, and builds a separate encoded run schedule.
Runs merge only when adjacent verified SHA-256/byte-length identity, dimensions,
placement, and the compose contract's no-blend/no-disposal behavior match. S3 keys
may differ when the encoded content is identical. Nonadjacent repeats remain in
their original positions. Every referenced object still has its checksum, length,
still-image status, and actual encoded dimensions verified, even when its frame
record was merged. No pixels are decoded/re-encoded in production finalization.

The current compose contract emits metadata-free still WebPs. Unknown frame
fields and embedded ICC/EXIF/XMP metadata are rejected rather than silently
introducing different blending, disposal, color, or orientation behavior. A future
contract extension must explicitly define those semantics.

Merged durations are limited to `0xffffff` milliseconds. Oversized runs are split
without changing total duration. Original frames of 10 ms or less are left
unmerged because decoders may clamp these durations; splitting does not create
new tiny fragments. This follows the timing/flags boundary in the
[WebP container specification](https://developers.google.com/speed/webp/docs/riff_container#animation).

When multiple logical frames collapse into one run, Rust writes a timed,
single-ANMF animation by copying the compressed image chunks. This avoids
`webpmux` simplifying it into an untimed still image. Genuine one-logical-frame
requests retain the existing still-image behavior. Muxed output is validated
against the **physical** duration/geometry/flags schedule, canvas, transparent
background, and infinite loop before any final image or cache metadata is
published.

Result/cache metadata keeps `frame_count` as the logical count and adds
`logical_frame_count`, `physical_frame_count`, `merged_frame_count`, and
`finalize_policy`. `finalize_profile` also reports unique downloads, animation
duration, download time, and mux time. The new `webp-adjacent-runs-v1` policy is
included in the final render hash; existing vector, bounds, and component cache
identities are unchanged. No cache objects are deleted. The optimization is
automatic once deployed; there is no new request flag.

The actual Rust finalizer was tested locally with the saved current Alina output:
**16,586,468 → 4,173,156 bytes; 120 → 30 physical frames; 120 logical frames;
1206×2048 canvas; 5,000 ms.** Pillow/libwebp independently decoded both complete
timelines and verified identical RGBA over every overlapping time interval. The
74.84% byte reduction agrees with the audit's scratch experiment. This is an
output-correctness/size result, not a measured AWS latency improvement. Local mux
tests used Homebrew libwebp 1.6.0; the deployed container remains pinned to 1.5.0.

Additional tests cover lossy/lossless transparent pixels, all-identical animations,
genuine stills, adjacent/nonadjacent repeats, changed placement/dimensions,
cross-batch ordering, 24-bit splits, tiny durations, malformed containers,
duplicate/missing records, checksum/length/dimension errors, and rejection before
publication. The normal Rust suite and the full Rust-to-WebP integration pass.

```sh
cargo test --manifest-path services/pipeline-rust/Cargo.toml

# Use installed tool paths and a Python environment containing Pillow.
AQW_TEST_CWEBP=/path/to/cwebp \
CHAR_RENDER_WEBPMUX=/path/to/webpmux \
AQW_TEST_PYTHON=/path/to/python \
cargo test --manifest-path services/pipeline-rust/Cargo.toml \
  --test pipeline adjacent_runs_preserve -- --ignored --nocapture

# Replay a saved full-replacement/no-disposal animation through finalization.
CHAR_RENDER_WEBPMUX=/path/to/webpmux \
AQW_TEST_PYTHON=/path/to/python \
AQW_TEST_RUN_SOURCE=/path/to/original.webp \
AQW_TEST_RUN_OUTPUT=/path/to/new-output.webp \
cargo test --manifest-path services/pipeline-rust/Cargo.toml \
  --test pipeline adjacent_runs_on_saved_animation -- --ignored --nocapture
```

The optional output path is written by the replay test, along with a `.json`
sidecar; choose a new filename. The retained implementation-test artifact is
`/private/tmp/aqw-adjacent-runs-20260906.PfMW9E/alina.webp` (temporary storage).

## 2. Large empty assets: partly upstream, partly a current pipeline gap

**There is evidence for both an oversized FFDec export and avoidable work in our pipeline.** It is not accurate to blame every case on the SWF parser.

The saved `PetKittenBOOMBlack` export at `/private/tmp/cosmiq-pet/pet.svg` declares **1838×2322.4**, while the existing Rust bounds probe finds conservative visible bounds of approximately **72.43×81.65**: about **722 times less area**. The empty space already exists in the raw FFDec SVG. Whether its original cause is timeline-wide SWF bounds or FFDec's own bounds calculation remains unproven.

Current code then unnecessarily carries that loose rectangle into raster allocation:

1. [finish.rs, around lines 250–268](../services/pipeline-rust/src/finish.rs) uses measured per-state bounds to choose the shared canvas.
2. Component tasks created around line 311 do **not** carry those visible bounds.
3. [import.rs, around line 597](../services/component-raster-rust/src/import.rs) reconstructs `imported.bounds` from the FFDec header.
4. [component_svg.rs, around lines 130–162](../services/component-raster-rust/src/component_svg.rs) sizes the component page from that rectangle.
5. [worker.rs, around lines 452–463](../services/component-raster-rust/src/worker.rs) rasterizes it and only then crops to alpha.

For the saved pet placement, the existing canvas clamp limits the page to **4096×2139**. Carrying the measured bounds through with the existing 24px margin predicts approximately **1081×1017**, or **eight times fewer page pixels**. This is an allocation calculation, not a measured eightfold speedup; internal filter work and fixed costs will affect the result.

This is a strong candidate for preserving identical final pixels while reducing CPU and memory. Preserve the same global scale, integer pixel origin, and downsample grid. Validate the bounds after considering minimum-stroke corrections, color/alpha transforms, filter extents, and clipping. A low-resolution raw SVG probe alone is not a mathematical guarantee for all later transformed artwork.

There is also a **reproduced invisible-state framing regression**. The current [bounds.rs, around lines 265–283](../services/pipeline-rust/src/bounds.rs) restores the complete declared page whenever both probes render empty. That conservative fallback protects tiny/faint artwork, but also includes provably invisible artwork. An SVG whose sole rendered group has `opacity="0"` and a 1431×1566 header returns `uncertain` with **1443.23×1578.23** bounds.

The August 30 `9478ffa` fix documented this same class of phantom margin for MightyAweCapeCC; the Rust migration needs a sound equivalent. Distinguish **proven invisible** from **unresolved tiny/faint content**, using the reachable rendered tree and filter/alpha semantics. Do not blindly discard everything that an initial thumbnail misses, and do not simply copy the old opacity regex. This may improve framing and effective line detail as well as speed. It was not established as the cause of Annie's present output.

Scratch reproduction: `/private/tmp/aqw-bounds-audit-2026-09-06/fully-invisible.svg`, inspected with the already-built `aqw-render-pipeline probe-svg` command. The relevant source behavior was also verified directly; no binary was rebuilt or source edited.

### Implementation addendum: measured component bounds and invisible framing

Implemented locally after the audit on September 6; not deployed. New
`PrepareFinish` tasks carry `raster_bounds` in FFDec registration space, with
the `prepared-tree-region-v1` policy. Legacy component manifests without this
field remain readable and retain the original allocation path.

The raster worker imports/customizes the SVG and calibrates minimum strokes
as before. It parses that **same prepared viewport once**, then unions the probe
hint with conservative prepared-tree stroke/filter extents and the existing
24-raster-pixel margin. The thumbnail alone never authorizes clipping. It does
not rebuild a smaller viewBox, change global scale, or recalibrate strokes on a
different page. Integer crop offsets are carried into alpha cropping and the
existing output-grid downsampling.

The crop is guarded against the pinned resvg implementation's canvas-dependent
layer allocation limits. Every isolated surface must retain its dimensions and
local transform, and path/gradient sampling transforms must remain identical.
If moving the page origin would change those calculations, the worker tries
candidates retaining one or both original origins. Unsupported masks, clip
paths, patterns, text/images, and `feImage` subroots conservatively retain the
old page. So do changed filter caps and crops saving less than 10% of the page.
Authored filters are not rewritten or arbitrarily shrunk; that remains item 3.

The saved `PetKittenBOOMBlack` fixture, with its frozen 4096-raster/2048-output
placement and authored colors, now allocates **1427×2139 = 3,052,353 pixels**,
instead of **4096×2139 = 8,761,344 pixels**: **65.16% fewer page pixels**.
The worker's final PNG bytes, placement, dimensions, and downsampled pixels
match exactly. This is a guarded 2.87× page-area reduction, not the original
8× estimate and not an AWS latency claim. The larger-than-estimated crop retains
full prepared effect bounds and an original origin needed for sampling parity.
Local debug replay timings varied substantially and are not a performance
forecast.

Bounds probing now uses a bounded, namespace-aware DOM/reference traversal to
prove structural invisibility: unreachable definitions do not count as painted
artwork; reachable `<use>` targets are resolved; zero group opacity is applied
after its own filters; an ancestor filter can still generate alpha. Ambiguous
references, cycles, CSS/style overrides, and unsupported constructs do not
produce a proof. A thumbnail miss or tiny/faint geometry still retries and
retains the conservative page. The final prepared component can also skip
rasterization when the same structural proof succeeds.

The probe retains the declared registration-space page separately. Positive
authored alpha offsets can invalidate raw-SVG bounds/visibility, so those parts
union the declared page with any measured/padded bounds for framing and the
allocation hint. An end-to-end test
checks that an invisible 1431×1566 cape frames exactly like an absent cape,
while an unresolved faint cape still enlarges the conservative canvas.

`component_raster_region` logs the policy, selected/fallback reason, original
page dimensions/pixels, and allocated dimensions/pixels. Bounds policy is now
`resvg-0.48.1-aqw-v1-cells-v2-visibility`; component cache schema is `3`, scoped
to the bounds hint and region policy. Task and final-render identities include
the changed policy/inputs. Existing source/vector exports and animation metadata
remain reusable; no cached objects are deleted. This is automatic after the
owner deploys, with no new request flag and no architecture/dependency changes.

Validation includes pixel-exact filtered/tinted/rotated/mirrored/gradient
fixtures, deliberately incomplete thumbnail bounds, faint remote marks,
alpha-generating filters, guarded subroots/filter-cap changes, invalid hints,
legacy manifests, cold/warm cache separation, framing, and the saved pet replay.
The raster and pipeline test suites pass. Strict raster Clippy passes; strict
pipeline Clippy still reports pre-existing lints in `swf.rs`, `webp.rs`, and an
`export.rs` test, unrelated to this implementation.

```sh
cargo test --manifest-path services/component-raster-rust/Cargo.toml
cargo test --manifest-path services/pipeline-rust/Cargo.toml

# Frozen local SVG and the matching saved prepare manifest; no AWS access.
AQW_TEST_REGION_SVG=/path/to/pet.svg \
AQW_TEST_REGION_MANIFEST=/path/to/manifest.json \
cargo test --manifest-path services/pipeline-rust/Cargo.toml \
  --test oversized_region -- --ignored --nocapture
```

## 3. Reduce filter work while preserving the effects

The dragon is expensive because many filtered surfaces are rendered repeatedly, not because its SVG is especially large on disk. The September 5 [Annie investigation](annie-dragon-raster-investigation.md) counted 69 authored Gaussian blurs and 23 authored filters, each with a 300%×300% region. The worker also adds color filters. A filter rectangle can include nine times the object's bounding-box area before clipping, even where most pixels are transparent.

There are two different opportunities:

- **Our generated color matrices:** [component_svg.rs, around line 34](../services/component-raster-rust/src/component_svg.rs) gives generated filters similarly broad regions. Alpha-preserving tint/darken matrices are pointwise operations and do not create a blur halo. Restrict intermediate work to the actual input effect/stroke bounds or implement an equivalent pointwise renderer path. Account for nested glows and blending; the input extent may exceed the geometric shape bbox.
- **Authored blur/filter chains:** derive required regions from painted content and each primitive's effect extent. Preserve filter coordinate systems, strokes, offsets, masks, and edge behavior. The current renderer can still admit intermediate regions up to five canvas widths and heights; tight final output alone does not constrain every intermediate allocation.

Earlier local Mac diagnostic replacement of all Annie filter regions with 200% regions reduced correctly scaled 4096 raster time from **42.06 s to 19.24 s**. This is evidence that filter area matters, **not permission to shrink every filter to an arbitrary percentage**. That variant has not passed pixel/visual comparisons and may clip effects.

Investigate the small-sigma blur path as well. The current vendored [resvg filter dispatch](../services/component-raster-rust/vendor/resvg-upstream/crates/resvg/src/filter/mod.rs) uses libblur only when the selected box path has **both transformed sigmas ≥2**. Smaller sigmas use upstream IIR. A source sigma can cross that threshold when resolution changes. Correctly scaled local Mac Annie screening measured **42.06 s at 4096×2699 but 67.93 s at 2048×1350**. The dispatch threshold is a plausible explanation, not a per-primitive profiling result.

Consequently, do not promise that halving raster dimensions makes this asset four times faster. First measure each blur implementation's calls, sigmas, allocated pixels, and time. A replacement small-sigma algorithm needs a quality comparison; box approximations are not automatically equivalent to the existing filter output.

## 4. Cache expensive colored components, and reuse stable subtrees carefully

The existing [component cache](../services/component-raster-rust/src/cache.rs) only admits parts with no color rules **and** no authored placement colors. This is narrower than “cache this exact appearance.” It excludes precisely many of the expensive cosmetics:

| Inspected request | Eligible component tasks under current rule | Actual component hits |
| --- | ---: | ---: |
| Dalvi `5a400e21` | **0 / 418** | 0 |
| Akine `6c7c0336` | **0 / 215** | 0 |
| Annie `1aa28d78` | 26 / 98; expensive ground excluded | 0; cache disabled in this request |

Annie's ground has 26 color rules; Dalvi's cape has three rules and nine placement-color entries. These are manifest observations, not estimated cache rates.

Add a later exact-appearance cache keyed by **all resolved rendering inputs**, including actual relevant color values, authored transforms, state signature, renderer/export policy, filters, placement, canvas scale/grid, and sampling settings. Immutable authored colors can be part of a deterministic key. A cache hit can retain identical PNG bytes. This helps repeat appearances and repeated item/color combinations, not a first-ever render. Current keys also include the global viewbox and layer context, which limits sharing between differently framed characters; removing those inputs without replacing their effects would be incorrect.

For first renders, reusable filtered subtrees offer a larger architectural gain. The saved dragon frames share path/filter definitions and vary only a handful of transforms, although some stroke widths also change. Rendering stable expensive pieces once and compositing them later could avoid dozens of repeated blurs. Start with exact repeats or integer translations at a fixed pixel grid. Arbitrary rotated/scaled bitmap reuse changes sampling and potentially filter semantics; evaluate it as a separate quality-controlled optimization. Do not remove repeated `<use>` nodes solely because their XML matches: alpha and antialiased edges can accumulate.

Final-result caching is already implemented but disabled by default in dev tuning. A production policy can enable it for identical requests after confirming complete cache identity/versioning. Prewarming popular immutable vector states is also useful: it removes Akine-like export misses, although it does not address Annie's current cached-export raster bottleneck.

## 5. Improve scheduling before buying more compute

The account already has **1,000 regional Lambda concurrency**, but the default inline raster Map runs at **40**. Distributed raster mode already exists with a configured ceiling of **200**. The application fan-out limit and the account quota are separate.

I simulated scheduling using the actual per-task CloudWatch handler durations, maintaining each task's measured cost. This excludes cold starts, Step Functions overhead, throttling, and changes in performance under greater parallelism, so it is a screening estimate, not a deployment forecast:

| Request | Current-order 40-worker simulation | Longest-first 40-worker simulation | Current-order 200-worker simulation |
| --- | ---: | ---: | ---: |
| Dalvi | 37.27 s | **27.51 s** | 27.51 s |
| Akine | 17.33 s | 16.44 s | **13.89 s** |
| Annie | 105.27 s | 104.50 s | **74.91 s** |

Dalvi is a good candidate for scheduling predicted expensive raster tasks first. Prior timings and estimated filter area are better cost signals than frame index. Task execution order can change while the manifest preserves the authored composition order. Annie has 49 genuinely slow ground states; 40 slots require another wave, and even 200 workers cannot beat the slowest ~75-second handler without improving that handler.

Test existing distributed mode against inline mode on the same frozen request/cache state before changing defaults. It adds child workflow overhead and changes orchestration cost. More concurrency does not multiply compute cost if aggregate billed work stays equal, but starts, S3 traffic, and workflow charges can differ. Keep per-job fairness when several Discord requests run together.

## 6. AVIF can meet the size goal, but preserve temporal compression and CPU budget

The existing [`benchmark_webp_avif.py`](../scripts/benchmark_webp_avif.py) results in `tmp/annie-webp-avif-comparison/results.json` compare a 120-frame, five-second, 2048×1350 sequence. The source is the 110.13 MB **lossless WebP** from AWS.

| Full-animation local result | Bytes | Encode time | Qualifications |
| --- | ---: | ---: | --- |
| `img2webp` lossy Q85 / m4 | 26,039,656 | 49.59 s | Whole animation encoder, not current parallel Lambda wall time |
| `img2webp` lossless | 102,587,602 | 123.14 s | Visible colors/alpha exact in sampled checks; hidden transparent RGB not all preserved |
| AVIF Q60 / speed 6 / all CPU threads | **6,039,165** | **20.26 s** | Prior all-core local result |
| AVIF lossless / speed 6 / all CPU threads | 59,055,107 | 34.15 s | Sampled RGBA exact; still far over the size target |

The existing Q60 animation's sampled white-composited comparison reports PSNR 45.23 dB, SSIM 0.99656, and exact alpha. Those are promising measurements, not proof that every line or animation frame is visually lossless.

To expose the CPU allocation issue, I repeated the AVIF sequence encode with **two encoder threads**, explicit YUV 4:4:4 and alpha quality 100:

| New local ARM64 run | Bytes | Time |
| --- | ---: | ---: |
| Q60 / speed 6 / `-j 2` | 6,039,165 | **90.70 s** |
| Q60 / speed 8 / `-j 2` | **7,632,867** | **26.76 s** |
| Q70 / speed 8 / `-j 2` | **9,612,139** | **37.30 s** |

All 120 decoded alpha planes matched the source exactly for each of the three new AVIF files. Color quality still varies materially with encoder speed. Using the same twelve source samples, the new comparison gave:

| Local AVIF profile | White-composited PSNR | White-composited SSIM |
| --- | ---: | ---: |
| Q60 / speed 6 | 45.21 dB | 0.99654 |
| Q60 / speed 8 | 40.34 dB | 0.98778 |
| Q70 / speed 8 | 42.48 dB | 0.99218 |

The old WebP Q85/m4 animation reports 41.24 dB / 0.98997. The new tests use Pillow decoding while the older AVIF results use avifdec, so small differences between old/new measurements are not necessarily encoding differences. Q60/speed 8 is **not equivalent quality** to Q60/speed 6 and does not beat the old WebP SSIM result. Q70/speed 8 is a better candidate, but its foreground-only PSNR is 39.84 dB; no visually lossless claim is established. Increasing speed changes quality as well as time and bytes.

These are single local runs on an eight-logical-CPU M1 Mac, not Lambda predictions or strict CPU isolation. The speed-8 Q60 encode is 3.39× faster than speed 6 with the same thread cap, but **26% larger**. It remains below the desired attachment budget for this fixture; it should not be described as a speedup with unchanged bytes. Q70/speed 8 also fits this particular sample, with limited headroom. Larger assets such as Dalvi still need measurement.

The big AVIF gain comes from encoding a **sequence**, not replacing each independent `cwebp` output with an independent AVIF. In the old twelve-independent-frame sample, AVIF lossy totaled 2.49 MB against WebP's 2.93 MB—far less dramatic than the full sequence. [libavif's sequence documentation](https://github.com/AOMediaCodec/libavif/wiki/Sequences) describes sequence encoding and timing.

A useful future integration should:

1. Feed the original composed RGBA into the selected encoder. Avoid generating lossy WebP, decoding it, then encoding AVIF again.
2. Preserve alpha, dimensions, frame durations, and loops. Make 4:4:4 chroma explicit for colored line art; do not assume equal Q numbers imply equal quality across codecs.
3. Evaluate a CPU encoder worker receiving frames as soon as they are available, overlapping composition/download with encoding. It still needs sufficient real CPU; the present 3,008 MiB Lambda allocation is not the all-core local machine.
4. Compare whole-sequence encoding with a small number of independent sequence segments only if the single encoder remains the critical path. Segment boundaries reduce temporal reuse and complicate muxing/timing; benchmark the resulting complete file. Do not assume AVIF segments can be concatenated like individual WebP frames.
5. Keep the selected profile within a byte budget with margin. Select from measured profiles; repeated full encode attempts can consume the latency savings.

The WebP alternative is an animation-aware delta encoder (`WebPAnimEncoder`/`img2webp`) or correctly implemented changed-region frames, preserving transparent clearing and disposal. The existing full-animation WebP benchmark already shows that this alone does not bring Annie under 10 MB. A serial `img2webp` finalizer also gives up much of the current parallel encoding advantage. [Google's img2webp documentation](https://developers.google.com/speed/webp/docs/img2webp) documents mixed lossy/lossless mode and near-lossless preprocessing; these are candidates to measure, not demonstrated wins on the current render path.

There is also a smaller, quality-preserving encoder-path improvement before a codec migration. [The compose worker, around line 455](../services/component-compose-rust/src/worker.rs) converts composed RGBA to PNG, writes it, then launches `cwebp`, which must decode that PNG again. [png.rs](../services/component-compose-rust/src/png.rs) uses balanced PNG compression and adaptive filtering. A direct libwebp RGBA API can remove this intermediate compression/decompression and file round trip with the same final codec settings; a faster temporary PNG mode is a smaller alternative. Verify the same libwebp configuration and final output. A larger temporary PNG does **not** imply a larger delivered WebP. The current `encode_ms` combines these operations, so profile them before assigning a speedup estimate. This is a real code opportunity, not a measured production gain.

For sharp lines, also benchmark WebP near-lossless and suitable encoder effort rather than only Q85 versus fully lossless. The historical [cwebp benchmark note](cwebp-qmlossless-benchmark.md) says Q is ignored in lossless mode; the [official cwebp documentation](https://developers.google.com/speed/webp/docs/cwebp) says it controls compression effort. The near-lossless option changes pixels; it is not a byte-exact codec mode.

## Quality and Discord delivery constraints

Lossless encoding preserves its **input raster**, including any rendering artifacts. It does not make the renderer faithful to Flash, reverse an approximate blur, or restore detail lost during rasterization.

I visually inspected the existing Annie decoded input and lossless AVIF round trip. Both contain suspicious rectangular gaps and thin colored horizontal/vertical lines around the dragon. Their cause was not established here. A codec benchmark that preserves those pixels is useful for compression comparisons but cannot establish correct SWF rendering. Capture original composed RGBA and compare these regions against a trusted runtime/reference before declaring the output quality solved.

The sibling repository contains AIR-based rendering/try-on tools, and the SDK was located at `../airsdks/AIRSDK_51.3.1` rather than the initially suggested location. Those tools are useful as a separate visual reference for timeline/filter behavior. Replacing the whole production exporter/runtime would be a much larger compatibility and operational project; there is no new benchmark here showing it beats the current CPU pipeline.

For later quality acceptance, use Alina, Annie, Dalvi, Akine, the oversized pet, a faint glow, an opacity-zero state, a static image, and representative nested idle timelines. Keep 2048 output and unchanged animation timing. Compare against original RGBA on white, Discord-dark, and checkerboard backgrounds; inspect foreground/edge-only error and alpha in addition to whole-canvas PSNR/SSIM, because transparent background can inflate global scores. Review the motion and the actual Discord preview as well as individual still frames.

Discord officially supports animated AVIF through attachments and embeds and describes transcoding AVIF to WebP for display. Thus attachment quality and the preview's decoded quality are separate checks. [Discord's format rollout](https://discord.com/blog/modern-image-formats-at-discord-supporting-webp-and-avif)

The current developer reference specifies a default **10 MiB per-file** upload limit, with possible higher limits, rather than precisely 10,000,000 bytes. A conservative delivery profile should leave margin and use the actual interaction attachment limit where available. Small upload bytes also reduce transfer latency; they do not guarantee that the Discord-transcoded preview is lossless. [Discord upload reference](https://docs.discord.com/developers/reference#uploading-files)

There is no evidence here that arbitrary 120-frame 2048px animations can always be truly lossless, below 10 MiB, and fast. The measured lossless Annie AVIF is still 59.06 MB. The realistic primary delivery target is visually near-lossless color with exact alpha, retaining 2048px and full timing; true lossless can remain available where the actual result fits. In particular, test duration merging before rejecting a lossless Alina-like animation: the current unmerged container can substantially overstate the bytes actually needed.

## AWS capacity and cost

Read-only AWS APIs confirm **1,000 regional/unreserved concurrency** and **all ten deployed functions on ARM64**. The old ten-concurrency comment in [environment.ts](../lib/config/environment.ts) predates the successful August 28 quota increase. No further regional concurrency request is justified by the cases above.

Prepare, export, raster, compose, and finalizer currently use 3,008 MiB; bounds uses 1,024 MiB. The repository's claim that the account is *still restricted* to 3,008 MiB was **not independently verified**: current account settings and listed Lambda Service Quotas do not expose that memory ceiling. The standard ceiling is 10,240 MiB, and AWS documents CPU allocation proportional to memory, with approximately one vCPU at 1,769 MiB. [AWS memory configuration](https://docs.aws.amazon.com/lambda/latest/dg/configuration-memory.html), [new-account limits](https://docs.aws.amazon.com/lambda/latest/dg/gettingstarted-limits.html)

Raising memory alone is not the first fix. Raster blur explicitly uses `RESVG_BLUR_THREADS=single`; `cwebp` is invoked without `-mt`. Additional cores need a workload that uses them. Test 1,769 / 2,048 / 3,008 MiB for the current serial paths, and larger sizes for genuinely multithreaded codec/renderer experiments if available. An older August 28 commit, `dbefa818`, recorded essentially unchanged prepare timings at 5,308 versus 3,008 MiB (13.4 versus 13.1 s); that was the previous pipeline, but illustrates why measurement matters.

Live Oregon pricing queries returned ARM Lambda tier-one compute at **$0.0000133334 per GB-second** and Standard Step Functions at **$0.000025 per transition**, before free allowances/credits. [Lambda pricing](https://aws.amazon.com/lambda/pricing/), [Step Functions pricing](https://aws.amazon.com/step-functions/pricing/)

```text
Compute cost = sum(memory MiB / 1024 × billed seconds) × ARM GB-second rate
Workflow cost = billed Standard transitions × transition rate
```

At 3,008 MiB, 1,000 aggregate billed seconds are about **$0.03917** of Lambda compute. Annie's ~2,820 aggregate raster-handler seconds imply roughly **$0.11** of raster compute using handler time as a proxy; this is not a full billed-job total. Init/rounding, other stages, requests, S3, logs, storage, and workflow charges are additional. Five hundred Standard transitions cost $0.0125. Optimize both latency and aggregate work; CPU-only does not mean expense-free.

Moving from 3,008 to 4,096 MiB requires duration below **73.4%** of baseline merely to break even on compute. A move to 5,308 MiB requires below **56.7%**. Provisioned concurrency or an always-on GPU is unnecessary for the improvements identified here.

A useful account request, **if the memory restriction is confirmed**, is:

> Please confirm whether this account still has the new-account 3,008 MiB maximum Lambda function-memory restriction in us-west-2. If so, please lift it to the standard 10,240 MiB maximum so we can benchmark CPU-only image rasterization and encoding at several memory sizes. Regional concurrency is already 1,000; no concurrency increase is requested. Production sizing will follow measured duration and GB-second cost.

No request was submitted. Support case contents could not be retrieved with the Support API because that API returned `SubscriptionRequiredException`; this was a support-plan entitlement limitation, not an approval rejection. No paid support plan is recommended just to obtain this information.

## How the historical notes map to today's implementation

The relevant `logs/` directory is in `../aq-image-search`. Local log modification timestamps are useful clues, but commit dates and deployed execution contents are stronger evidence of which implementation ran. These milestones prevent spending time redoing completed work:

| Date / commit | What changed | Relevance now |
| --- | --- | --- |
| Aug 29 `030ca57` through `511f7e9`, `bd34914` | Removed duplicate transfer/work; vector cache; alpha-probed bounds | Much of the original speed-review checklist is already done |
| Aug 30 `9478ffa` | Invisible states no longer enlarge the shared canvas | Same failure class is reproducible in current Rust fallback |
| Aug 30 `22d1691` | Separate supersampled raster and delivery sizes | Bot requests commonly use 4096→2048; dev defaults alone are misleading |
| Sep 2 `19aaa3c`, `7e256df`, `e6ec314` | Rust composition, ARM raster, SIMD blur | Already implemented; not new recommendations |
| Sep 2 `414a0b9` | Clamp component page to the shared raster canvas | Prevented the enormous pet allocation; unused blank area remains inside clamp |
| Sep 4 `45da4b7` | No-color component cache | Current eligibility excludes many expensive assets |
| Sep 4 `1c307b6` | Large-sigma libblur box path | Small-sigma fallback still merits profiling |
| Sep 5 `e6a092e`, `1defc61` | Rust orchestration and streaming bounds prefetch | Current AWS paths already benefit; cached export is subsecond in several jobs |
| Sep 5 `945cbf1`, `07e6974` | Recipe deduplication and adaptive composition batching | Avoids encode work, but final container still repeats adjacent payloads |
| Existing uncommitted work | Nested timeline normalization, bounded raster results/collector, restart tooling | Included in source review; later AWS runs contain the collector, but commit timestamp alone cannot identify the complete deployed source |

The older [ablation notes](resvg_bottleneck_ablation_plan.md) must be read with the newer [Annie correction](annie-dragon-raster-investigation.md). `bench-svg`'s width/height helper changes the viewport and adds a root scale; on an assembled SVG with a viewBox this double-scales artwork. Historical **16×/180× resolution speedup claims are not valid evidence**. Use correct viewport scaling without changing the artwork twice before drawing quality/performance conclusions.

The existing [render_timing_report.py](../scripts/render_timing_report.py) also predates the current architecture. It references the removed render log group, omits current raster/compose/export profiles, and does not record `MapStateEntered` when calculating Map durations. Concurrent child states sharing a name also need proper execution/iteration pairing rather than a single start timestamp per name. This report uses outer Map timestamps for workflow stage time and CloudWatch profiles for individual workers; the scratch history summary's child-state aggregates are not used as worker timing evidence. Do not use its current output as the sole performance baseline.

## Recommended sequence for later implementation

| Priority | Work | Expected effect on final bytes / quality | Evidence level |
| --- | --- | --- | --- |
| 1 | Adjacent-identical WebP run merging | Same displayed pixels/timing, substantially smaller when applicable | **Verified on current AWS Alina output** |
| 2 | Carry validated component bounds; prove invisible states | Same target pixels, less empty allocation; improved framing for affected states | Current code gap + real asset allocation + synthetic regression reproduced |
| 3 | Direct RGBA encoding; exact-appearance cache; schedule expensive raster tasks first | Same codec fidelity; shorter repeated jobs and better task packing | Encoder round trip found in code; cache exclusions measured; scheduling simulated |
| 4 | Filter intermediate/pointwise color work and small-sigma profiling | Potential large CPU savings; effect fidelity must be validated | Dominant raster cost measured; algorithmic gains not yet validated |
| 5 | Direct RGBA→animated AVIF at a measured 4:4:4/alpha-exact profile | Major byte reduction; near-lossless color candidate | Local full-sequence benchmarks; Lambda integration unmeasured |
| 6 | Stable filtered subtree reuse | Potential large first-render savings | Geometry evidence; transformed-bitmap quality unproven |

Treat measurement as part of each change. Record source/renderer digest, full request, cache hits, resolution and actual page dimensions, physical/logical frame counts, output bytes, end-to-end wall time, billed GB-seconds, memory high water, and throttles. Raster telemetry's current `raster_pixel_count` is the **cropped/downsampled output rectangle area**, not the initial raster page or the sum of filter surfaces; add those distinct quantities when implementing profiling. Repeat the same frozen fixtures across cold/intermediate-cache/final-cache modes, and retain the output for quality comparison.

The first implementation does not need to sacrifice 2048px, shorten animation, rent a GPU, or promise a speculative codec speedup. There is already an exact current-output size win, a reproduced empty-state issue, and a concrete path to stop rasterizing blank pixels. Those are the best foundation for a fast, high-quality AVIF delivery path.

## Evidence locations

Raw execution inputs can contain Discord identifiers and appearance snapshots, so they remain outside the repository. Compact numeric findings needed to assess this report are included above.

```text
/private/tmp/aqw-render-speed-audit-2026-09-06/
  executions.json, summary.json
  <job>-execution.json, <job>-history.json
  <job>-componentraster-rust-logs.json
  <job>-componentcompose-rust-logs.json
  <job>-exportsource-logs.json, <job>-prepare-logs.json
  <job>-finalizer-logs.json, <job>-manifest.json
/private/tmp/aqw-codec-audit-20260906/
/private/tmp/aqw-bounds-audit-2026-09-06/
```

Full job IDs for reproduction with read-only AWS tools:

```text
Alina:       a28796e8-04d3-4c10-bb6e-9d72ca535145
Annie Q85:   1aa28d78-b90a-4712-9f2f-59d4a34dda8e
Annie LL:    8f82d491-e693-473f-8c29-fd4dc0673264
Dalvi:       5a400e21-8def-4fcc-92a4-675653d6ad20
Akine:       6c7c0336-94a3-497f-a221-aa72d80851b5
Older_flame: 1e92e66b-cf55-4b6c-8804-d6b384b2ff6f
```

Job artifacts have short S3 lifecycle retention and local temporary paths are not permanent. Preserve selected original-RGBA fixtures separately when implementing the changes. No claim of measured production improvement is made for a proposed optimization unless explicitly identified as verified above.
