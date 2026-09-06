# Consolidated render-performance priorities — September 6, 2026

This list replaces the implementation ordering in the [pipeline audit](render-speed-quality-audit-2026-09-06.md#recommended-sequence-for-later-implementation) and [raster follow-up](raster-performance-follow-up-2026-09-06.md#suggested-order). Those reports retain the detailed evidence and qualifications.

The objective remains fast, inexpensive CPU rendering with a 2048px longest output dimension, crisp lines, full animation timing, and small Discord attachments. Preserve the existing 4096→2048 sampling when comparing performance. Prefer pixel-exact execution improvements; evaluate near-lossless delivery separately from raster correctness.

**Already implemented: keep these changes**

The owner reports that the original priorities 1 and 2 are implemented. Their implementation addenda document local validation; this consolidated list does not establish deployment status or a new AWS speedup.

| Original priority | Completed work | What remains to measure |
| --- | --- | --- |
| 1 | Merge adjacent identical WebP frames while preserving timing. The saved Alina replay shrank from 16,586,468 to 4,173,156 bytes with identical displayed pixels. | Current-build AWS finalization latency and final attachment sizes across representative animations. |
| 2 | Carry validated component bounds and prove invisible states. The guarded pet replay used 65.16% fewer page pixels with identical final PNG bytes and placement. | Current-build AWS CPU/memory savings and remaining fallback frequency. |

Component bounds reduce the outer raster page. Filters can still allocate much larger internal surfaces, so completing the second item does not complete filter working-region optimization.

**Before the next optimization: establish the current AWS baseline**

The follow-up's AWS tests predate the new bounds implementation. Freeze representative Alina, Annie, Dalvi, Akine, oversized-pet, and faint/transparent fixtures; record the actual deployed renderer digest and configuration. Compare changes against that build, rather than combining historical speedup ratios.

Measure raster time separately from download, parse, downsample, PNG encoding, composition encoding, and workflow overhead. Record actual page/filter dimensions, blur-path calls and time, logical/physical frame counts, complete output bytes, billed GB-seconds, and memory. Separate cold starts, warm invocations, component-cache hits, and completed-render hits. A component PNG's size is not the final animation's size.

Use M1 tests for correctness and candidate screening. Choose production changes using repeated AWS comparisons at matched memory, threads, resolution, inputs, and cache state.

**New implementation order**

| Priority | Work | Main benefit | Evidence and boundary |
| --- | --- | --- | --- |
| **1** | Implemented locally: validate the small-sigma IIR traversal improvement on AWS. | Less blur CPU on difficult first renders, preserving the existing algorithm. | 512 optimized local cases preserve recurrence bits and RGBA exactly. Deployment and AWS performance validation are pending. |
| **2** | Specialize simple color matrices and derive conservative filter working regions. | Fewer pixel passes and fewer transparent pixels processed inside filters. | AWS region/tint ablations identify substantial work, but those altered effects and are not deployable optimizations. |
| **3** | Improve filter-result lifetimes, ownership, and bounded scratch reuse. | Lower allocation/memory pressure and some redundant pixel work. | Copying is real; measured explicit clones were a minority of local render time. |
| **4** | Encode composed RGBA directly with libwebp. | Remove temporary PNG compression, file I/O, and decoding. | Existing pipeline opportunity; no measured AWS speedup yet. |
| **5** | Cache expensive components by their exact resolved appearance. | Faster repeated item/color combinations. | Previous eligibility excluded all inspected Dalvi and Akine components. Does not accelerate a first-ever cache miss. |
| **6** | Schedule expensive raster tasks first; then test existing distributed mode. | Shorter task queues and fewer long trailing waves. | Historical scheduling simulations support testing; concurrency cannot shorten the slowest individual task. |
| **7** | Integrate direct RGBA→animated AVIF using an AWS-measured quality/size profile. | Smaller high-resolution animations, including cases adjacent-frame merging cannot shrink. | Local sequence results are promising; production encoding latency and broader quality/size results remain unmeasured. |
| **8** | Reuse stable filtered subtrees or masks across frames. | Potentially avoid repeated expensive filters on first renders. | Larger architectural project with sampling, effect-context, and cache-identity challenges. |

This is a default order for reducing expensive first-render latency. Priorities 4–6 are independent opportunities and can proceed alongside renderer work; completing priority 3 is not a prerequisite. Start AWS AVIF profiling early as well—full codec integration does not need to wait for every raster optimization.

**1. Improve traversal before changing the blur algorithm**

Implemented in the repository's vendored resvg, with the original column traversal retained as a regression-test oracle. It changes vertical memory traversal while retaining each column's recurrence arithmetic. See the [implementation and AWS render runbook](iir-blur-traversal-2026-09-06.md) for validation and the frozen render commands; the implementation no longer depends on the scratch patch in temporary storage.

Compare decoded RGBA on AWS, including tiny and anisotropic sigmas, zero on one axis, random alpha, and narrow images. Instrument IIR and libblur separately. The local timings varied even for untouched code, so they do not establish an AWS speedup. Keep the existing large-sigma libblur improvement.

**2. Reduce matrix and internal-region work**

Start with specialized constant-tint and simple diagonal kernels, retaining a generic fallback. Combine demultiply/matrix/premultiply traversal where possible while preserving existing clamping and intermediate rounding. Keep filter isolation, color-space conversion, and effect ordering intact.

Derive working rectangles from actual input effect bounds and required output, with conservative blur support and fallbacks. Transparent-preserving pointwise operations offer a useful starting point; operations that generate alpha from transparent input need different treatment. Coordinate changes with the new component-bounds guards, especially canvas-dependent layer allocation and pixel-grid alignment.

The AWS dragon region ablation changed raster time from 43.89 seconds to 25.89 seconds, but it also changed pixels. The cape had a maximum channel difference of 127/255. These are evidence that area matters, not achievable pixel-exact speedup promises.

**3. Reduce unnecessary ownership and allocation work**

Resolve primitive dependencies and release or move results after their last consumer. Share immutable source inputs, mutate exclusively owned buffers where valid, and introduce capped scratch reuse after ownership is correct. Preserve named/default inputs, branching, clipping, and clearing semantics.

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
