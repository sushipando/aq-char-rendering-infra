# Consolidated render-performance priorities — September 6, 2026

This list replaces the implementation ordering in the [pipeline audit](render-speed-quality-audit-2026-09-06.md#recommended-sequence-for-later-implementation) and [raster follow-up](raster-performance-follow-up-2026-09-06.md#suggested-order). Those reports retain the detailed evidence and qualifications.

The objective remains fast, inexpensive CPU rendering with a 2048px longest output dimension, crisp lines, full animation timing, and small Discord attachments. Preserve the existing 4096→2048 sampling when comparing performance. Prefer pixel-exact execution improvements; evaluate near-lossless delivery separately from raster correctness.

**Already implemented: keep these changes**

Status updated after the owner's September 6 deployment and eight successful full-workflow render checks. The **original audit's priorities 1 and 2 are implemented**, as is **priority 1 in the new list below**. New priorities 2 and 3 are not implemented. The numbering changed between the two lists; measured component bounds are not the same work as internal filter-region optimization.

| Original priority | Completed work | What remains to measure |
| --- | --- | --- |
| 1 | Merge adjacent identical WebP frames while preserving timing (`9209cd4`). The saved Alina replay shrank from 16,586,468 to 4,173,156 bytes with identical displayed pixels; the latest AWS Alina output also has 30 physical frames and 4,173,156 bytes. | Broader attachment-size coverage and matched before/after finalization cost. |
| 2 | Carry validated component bounds and prove invisible states (`dad0305`). The guarded pet replay used 65.16% fewer page pixels with identical final PNG bytes and placement; the current AWS jobs exercised this implementation. | Isolated CPU/peak-memory savings and broader fallback-frequency measurements. |

Component bounds reduce the outer raster page. Filters can still allocate much larger internal surfaces, so completing the second item does not complete filter working-region optimization.

**Current AWS baseline and remaining measurement work**

The owner ran `scripts/test_iir_renders.sh` after deployment: three fresh-ID restarts each of frozen Annie and Dalvi, plus Alina and Akine. All eight succeeded. The compact bounds handoff also passed: Dalvi's 346 missing probes produced a 1,566-byte PlanBounds response instead of embedding full descriptors.

The recorded raster worker was ARM64, 3,008 MB, with `RESVG_BLUR_THREADS=single`; code digest `bed6a8fd0f210120148266f498c7fd35e1bf6d5b52d6a5460fd90fee13174270`. All 1,728 new raster profiles reported `cache_enabled=false` and `cache_hit=false`. Completed-render and bounds caches were also disabled; animation/vector reuse remained enabled. The 867 original raster profiles also recorded zero component-cache hits, although some original requests allowed caching.

| Character | Original longest individual RasterComponentStateInline | Latest longest, runs in order |
| --- | ---: | ---: |
| Annie | 75.03 s | 42.80 / 41.57 / 63.01 s |
| Dalvi | 27.67 s | 27.44 / 27.36 / 27.44 s |
| Alina | 1.24 s | 2.29 s |
| Akine | 14.26 s | 12.92 s |

These are individual state wall times, including invocation overhead, not whole-Map durations. They compare each job's worst task, not necessarily the same SVG/frame: task sets, bounds, and other code changed. They show combined workload outcomes, **not an isolated IIR speedup or an improvement for every component**. Annie's third run was warm and spent the extra time inside rasterization. Recorded PNG hashes and placements matched across repeats, but the complete outputs were not compared pixel-for-pixel against a pre-IIR build.

Operator logs are in `/tmp/aqw-iir-render-checks-km9USb`; the read-only comparison and raw evidence are in `/tmp/aqw-iir-results-review-qEiceQ/REPORT.md`. These temporary paths are not permanent fixture storage. Before the next change, preserve representative Alina, Annie, Dalvi, Akine, oversized-pet, and faint/transparent fixtures with their current manifests and deployed configuration. The earlier follow-up's isolated AWS tests predate measured bounds; do not combine their speedup ratios with this baseline.

Measure raster time separately from download, parse, downsample, PNG encoding, composition encoding, and workflow overhead. Record actual page/filter dimensions, blur-path calls and time, logical/physical frame counts, complete output bytes, billed GB-seconds, and memory. Separate cold starts, warm invocations, component-cache hits, and completed-render hits. A component PNG's size is not the final animation's size.

Use M1 tests for correctness and candidate screening. Choose production changes using repeated AWS comparisons at matched memory, threads, resolution, inputs, and cache state.

**New implementation order**

| Priority | Work | Main benefit | Evidence and boundary |
| --- | --- | --- | --- |
| **1** | Implemented, committed (`124a94c`), deployed, and exercised on AWS: small-sigma IIR traversal. | Targets blur CPU on difficult first renders while preserving the existing algorithm. | 512 optimized local cases preserve recurrence bits and RGBA exactly; all eight deployed workflow checks passed. An isolated AWS IIR speedup remains unmeasured. |
| **2** | Not implemented: specialize simple color matrices and derive conservative filter working regions. | Fewer pixel passes and fewer transparent pixels processed inside filters. | AWS region/tint ablations identify substantial work, but those altered effects and are not deployable optimizations. Split into matrix kernels and internal-region work below. |
| **3** | Not implemented: improve filter-result lifetimes, ownership, and bounded scratch reuse. | Lower allocation/memory pressure and some redundant pixel work. | Copying is real; measured explicit clones were a minority of local render time. No completed ownership/reuse implementation is waiting in temporary storage. |
| **4** | Encode composed RGBA directly with libwebp. | Remove temporary PNG compression, file I/O, and decoding. | Existing pipeline opportunity; no measured AWS speedup yet. |
| **5** | Cache expensive components by their exact resolved appearance. | Faster repeated item/color combinations. | Previous eligibility excluded all inspected Dalvi and Akine components. Does not accelerate a first-ever cache miss. |
| **6** | Schedule expensive raster tasks first; then test existing distributed mode. | Shorter task queues and fewer long trailing waves. | Historical scheduling simulations support testing; concurrency cannot shorten the slowest individual task. |
| **7** | Integrate direct RGBA→animated AVIF using an AWS-measured quality/size profile. | Smaller high-resolution animations, including cases adjacent-frame merging cannot shrink. | Local sequence results are promising; production encoding latency and broader quality/size results remain unmeasured. |
| **8** | Reuse stable filtered subtrees or masks across frames. | Potentially avoid repeated expensive filters on first renders. | Larger architectural project with sampling, effect-context, and cache-identity challenges. |

This is a default order for reducing expensive first-render latency. Priorities 4–6 are independent opportunities and can proceed alongside renderer work; completing priority 3 is not a prerequisite. Start AWS AVIF profiling early as well—full codec integration does not need to wait for every raster optimization.

**Effort and risk for priorities 2 and 3**

These are engineering assessments from the current code, not measured speedups or delivery-time promises. Difficulty includes correctness tests and representative AWS validation; 1/5 is a small isolated change and 5/5 is a renderer-wide correctness problem.

| Slice | Difficulty | Why | Safe first scope |
| --- | --- | --- | --- |
| **2a. Simple matrix fast paths** | **3/5 — medium** | A bounded pixel kernel, but demultiply/quantize/matrix/clamp/premultiply order and color space must remain exact. Even an identity matrix is not automatically equivalent to skipping the existing rounding steps. | Recognize constant RGB tint with alpha passthrough first, then diagonal matrices. Keep the generic implementation as fallback and test oracle. |
| **2b. Smaller internal filter working regions** | **5/5 — hard** | Changes filter-buffer coordinates, sampling, clipping, and boundaries throughout a graph. Blur, offsets, alpha-generating operations, and multiple inputs require different bounds rules. | Start with proven transparent-preserving pointwise operations and supported graphs; keep original allocation for unproven cases. No global percentage shrink. |
| **3a. Last-use release and ownership transfer** | **4/5 — medium-hard** | The executor currently retains a vector of results and uses shared `Rc` images. It needs correct dependency/use tracking before results can be moved or freed. | Resolve each input to its actual producer, count uses, and move only at the final use. Preserve branching, repeated names, implicit/default inputs, same-buffer two-input operations, and region metadata. |
| **3b. Bounded scratch-buffer reuse** | **3/5 after 3a** | Reuse is straightforward only after proving a buffer is no longer live. Stale pixels, wrong dimensions, and retained large allocations create correctness/memory risks. | Add a byte-capped, per-render pool with explicit clearing and an ordinary-allocation fallback. Measure whether reuse pays for its clearing/bookkeeping costs. |

Recommended implementation slices: **2a first**, then **3a** as the more contained structural change, followed by **2b**; add **3b** only if allocation profiles justify it. This splits the broad priority-2 package rather than requiring its hardest part to land before any ownership work. Each slice should be independently tested and benchmarked. The expected CPU benefit of 2a is more directly supported than clone removal alone; 3a may be valuable primarily for peak memory. None has a validated production speedup yet.

**1. Improve traversal before changing the blur algorithm**

Implemented and committed in the repository's vendored resvg, with the original column traversal retained as a regression-test oracle. It changes vertical memory traversal while retaining each column's recurrence arithmetic. See the [implementation and AWS render runbook](iir-blur-traversal-2026-09-06.md) for local validation and the frozen render commands; its original pre-deployment wording is historical, and the current AWS status is recorded above. The implementation no longer depends on the scratch patch in temporary storage.

Compare decoded RGBA on AWS, including tiny and anisotropic sigmas, zero on one axis, random alpha, and narrow images. Instrument IIR and libblur separately. The local timings varied even for untouched code, so they do not establish an AWS speedup. Keep the existing large-sigma libblur improvement.

**2. Reduce matrix and internal-region work**

Start with specialized constant-tint and simple diagonal kernels, retaining a generic fallback. Combine demultiply/matrix/premultiply traversal where possible while preserving existing clamping and intermediate rounding. Keep filter isolation, color-space conversion, and effect ordering intact.

Test against the existing kernel on transparent and low-alpha colors, clamping/negative coefficients, randomized premultiplied pixels, and both color spaces. Exhaust all 256 alpha values for supported constant-tint cases. Validate full rendered fixtures too: per-kernel equality does not prove correct dispatch or filter-context handling.

Derive working rectangles from actual input effect bounds and required output, with conservative blur support and fallbacks. Transparent-preserving pointwise operations offer a useful starting point; operations that generate alpha from transparent input need different treatment. Coordinate changes with the new component-bounds guards, especially canvas-dependent layer allocation and pixel-grid alignment.

Do not assume a fixed blur halo proves pixel equality: recursive IIR tails and changed buffer-edge initialization can change output when a surface is cropped. Unsupported or unproven filter graphs must keep their original working regions. This is the main reason 2b is substantially harder than 2a or the completed outer-canvas bounds work.

The AWS dragon region ablation changed raster time from 43.89 seconds to 25.89 seconds, but it also changed pixels. The cape had a maximum channel difference of 127/255. These are evidence that area matters, not achievable pixel-exact speedup promises.

**3. Reduce unnecessary ownership and allocation work**

Resolve primitive dependencies and release or move results after their last consumer. Share immutable source inputs, mutate exclusively owned buffers where valid, and introduce capped scratch reuse after ownership is correct. Preserve named/default inputs, branching, clipping, and clearing semantics.

The existing `Image::take` already avoids copying when `Rc::try_unwrap` succeeds, and the libblur wrapper already avoids its old input/output copies. Priority 3 must make ownership exclusive earlier; adding another `try_unwrap` is not the missing implementation. Test branch-and-merge graphs, duplicate result names, both operands referencing the same result, color-space conversion, `SourceAlpha`, and buffer clearing. Initially keep scratch reuse scoped to one render rather than retaining the largest buffers across warm Lambda invocations.

The temporary instrumented resvg contains timing/copy counters, not a last-use or scratch-pool implementation. The temporary IIR prototype is already incorporated. The region/tint ablations intentionally change pixels and are diagnostic evidence, not patches ready to port.

The local profile counted about 7.21 GB of explicit copies, taking 1.13 seconds of a 50.77-second render. That excludes other memory traffic, but it argues against treating clone removal alone as the primary CPU fix. Measure peak memory as well as duration.

**4–6. Keep the pipeline improvements from the first audit**

- **Direct encoding:** match current libwebp settings and validate decoded output and final bytes. A faster temporary PNG mode is an interim option; larger temporary files do not necessarily increase delivered WebP size.
- **Appearance caching:** include every relevant resolved color, transform, state, filter, renderer policy, scale, and sampling input. Keep cold and warm results separate. Do not remove framing/context inputs merely to increase hits.
- **Scheduling:** use measured or estimated filter cost, preserve composition order independently, and benchmark full workflows under realistic concurrent requests. Compare billed work and orchestration overhead before increasing fan-out.

**7. Preserve temporal compression in the AVIF path**

Feed original composed RGBA into a sequence encoder, avoiding a lossy WebP intermediate. Test explicit 4:4:4 color and exact alpha; preserve dimensions, timing, and loops. Overlap frame production and encoding where useful, and measure complete workflow latency rather than encoder throughput alone.

The local Annie Q60/speed-6 sample was about 6.04 MB; increasing speed changed quality and bytes as well as time. Its lossless AVIF was about 59.06 MB. Neither result guarantees a suitable profile for every asset. Select profiles on AWS using complete animations, attachment-budget headroom, edge/foreground inspection, and Discord preview checks. Do not equate identical quality numbers across encoders or speed settings.

**8. Reuse only rendering work whose identity is understood**

Start with exact repeated subtrees or integer translations at a fixed sampling grid. Include color, strokes, transforms, clipping, blending, and filter context. Arbitrary rotation/scaling of cached bitmaps and alpha-only glow shortcuts require separate quality validation.

**Defer or reject these approaches**

- **Blanket 200% filter regions:** reject; observed clipping/alpha differences make this unsuitable as a global rule.
- **Routing every small sigma into the existing box-blur path:** defer algorithm changes until after traversal profiling; small/anisotropic cases need explicit validation.
- **Algebraically collapsing matrices or repeated blurs:** not automatically lossless because intermediate clamping, quantization, regions, and edge behavior matter.
- **Generic SVG minification as a raster-speed project:** low priority; local dragon parsing took about 11 ms while rendering took tens of seconds. Removing proven dead rendering operations is a separate opportunity.
- **Broad ThorVG migration:** defer. The current limited matrix patch does not establish equivalent filter-graph behavior. Require a supported-feature gate and equal-feature AWS advantage before investing further.
- **Buying more compute as the first fix:** unnecessary for the listed experiments. Revisit memory/thread configuration only with measured latency and billed-cost comparisons; no GPU is required by this plan.

For execution-only changes, require pixel equality against the frozen baseline and check final encoded size. For approximate changes, explicitly assess alpha, crisp edges, motion, and final attachment bytes on light, dark, and checkerboard backgrounds. A faster renderer that clips a glow, changes line sampling, or produces a much larger attachment has not met the objective.
