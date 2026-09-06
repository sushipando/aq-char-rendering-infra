# Annie dragon raster investigation and Map payload failure

Date: 2026-09-05 (America/Los_Angeles).

## Single-thread rollback

`RESVG_BLUR_THREADS=single` is restored in CDK for the ARM64 component-raster
Lambda. The existing infrastructure assertion passes. Deployment is left to
the repository owner: `scripts/deploy_renderer.sh --yes`.

## SVG inspection

Examined Queen Annie 012's `LaeDWearNeonDragonr1` ground, task 30 in job
`8f82d491-e693-473f-8c29-fd4dc0673264`. The source SHA-256 is
`bd91fcb9e2f252cd1d8226d043f3fc872963e8dc4406e9d32029b50d6e615fb3`.
Godlow's gold dragon is a related asset; its earlier measurements are not an
exact benchmark of this SVG, appearance, and renderer build.

The 486,784-byte source contains 82 paths, 155 use nodes, 23 authored filters,
and 69 Gaussian blur primitives. Every authored filter has a 300% by 300%
region with x/y at -100%: nine times its object bounding-box area before
renderer clipping. Character assembly adds nine color filters.

The Rust worker's assembled page is 4096 x 2699 (11,055,104 pixels), already
clipped to the character canvas. The logged `raster_pixel_count` is actually
the cropped/downsampled result PNG area; it is not the initial raster page
area. The current telemetry name can therefore mislead performance analysis.

Comparing source frames 1, 6, and 45:

- All path geometry and filter definitions match.
- Seven use transforms change.
- Two stroke-width attributes change slightly; these cannot be ignored for
  claims of exact pixel equivalence.
- The source also has 24 pairs of identical consecutive use nodes. Removing
  them is not automatically safe: alpha and antialiased edges accumulate.

This supports investigating reusable rendered subtrees, keyed by appearance,
geometry, strokes, filters, scale, and relevant clipping/blending context.
It does not prove that transformed bitmap reuse will preserve current quality.

## Local screening measurements

Used the existing native ARM64 release `aqw-component-raster bench-svg` binary,
single-threaded blur, and the component SVG dumped by the Rust worker.
Preserved the viewBox and changed only viewport width/height for scaling.
The release binary predates the latest worker integration changes; these are
local renderer screening measurements, not a fresh Lambda deployment test.
Times include parsing and rasterization, not S3 or WebP composition.

| SVG variant | 4096 x 2699 | 2048 x 1350 |
| --- | ---: | ---: |
| Authored filters | 42.06 s | 67.93 s; repeat 68.30 s |
| Blur sigma set to zero | 17.35 s | 4.94 s |
| All 32 filter regions changed to 200% x 200%, x/y -50% | 19.24 s | 28.54 s |

Except for the repeated 2048 baseline, each entry is one screening run.
The modified-filter variants change rendering semantics and have not passed
pixel or visual comparison. They are not production optimizations yet.

An important complication is pixel-scale blur dispatch. The patched renderer
uses libblur only when both transformed sigmas are at least 2; smaller blur
uses upstream IIR. A common source sigma of 2, after the dragon's transforms,
is approximately 2.47 at 4096 but 1.24 at 2048. This is a plausible explanation
for the resolution reversal, not a per-primitive profiling result.

The old `bench::scale_svg` helper changes viewport dimensions and adds a root
scale while retaining the existing viewBox. For built component SVGs, that
scales the artwork twice and can change clipping. The earlier Godlow 16x/180x
resolution-speedup interpretation must be revalidated with correct scaling.

Next candidates:

1. Compute tighter filter regions from actual painted bounds plus effect
   extents; compare pixels/glows before applying. The 200% diagnostic suggests
   a roughly 2.2x improvement at 4096, not a safe universal percentage.
2. Reuse stable rendered body parts across animation frames, with explicit
   transform/filter/color correctness and image-quality checks.
3. Profile both blur dispatch paths before lowering raster resolution or
   spending more effort on threading.

## Failure: eccabdb9-d329-4e3d-a0a8-5b30eb7425df

This was Dalvi, with 418 unique component tasks. All 418 raster invocations
succeeded. The outer Inline Map failed assembling their output:

`States.DataLimitExceeded: RasterComponentStatesInline returned a result with
a size exceeding the maximum number of bytes service limit.`

| Payload | Compact JSON bytes |
| --- | ---: |
| Input to raster Map | 12,282 |
| Full result array | 358,633 |
| Combined Map output | 370,936 |
| Array with only current composer fields | 150,430 |
| Step Functions payload limit | 262,144 |

The failing deployment collected complete `RasterResult` records, including timing,
SVG statistics, long S3 keys, and other fields the compositor does not use.
The distributed branch collects the same records and has no ResultWriter;
changing the submitted mode alone does not solve this limit.

Verified in S3: 418 component result JSON objects, 417 nonempty PNGs, and the
547,338-byte preparation manifest. Job status is FAILED and its admission
slot was released. The manifest's expiration header is 2026-09-09 00:00 UTC;
job objects are governed by the two-day work-artifact lifecycle.

## Payload prevention and full restart

Implemented in Rust: each raster Map iteration returns scalar `0` before Map
aggregation; the aggregate is discarded. `CollectComponentResults` reads the
expected result JSONs with concurrency 16, validates identities/coordinate
spaces, and writes one compact S3 manifest. Compose receives its key and loads
the metadata from S3. Full per-task diagnostic records remain unchanged.

Normal path: `RasterComponentStatesInline` (or distributed) →
`CollectComponentResults` → `ComposeComponentFrameChunks` → `FinalizeAnimation`.

The compose-only resume feature was removed at the operator's request. Instead,
`--restart` clones the original hydrated execution request with a new UUID and
creation timestamp, retaining the appearance/asset selection, colors, settings,
cache policy, and Map modes. It uses normal admission and SQS submission, so the
current deployed workflow starts at `PrepareResolve` with all normal checks and
stages. No raster artifacts are copied. The original job stays unchanged.

```bash
scripts/render-character --restart eccabdb9-d329-4e3d-a0a8-5b30eb7425df --dry-run
scripts/render-character --restart eccabdb9-d329-4e3d-a0a8-5b30eb7425df --no-render-cache
```

Cache settings are preserved by default. `--no-render-cache` disables only the
finished-result shortcut; `--no-cache` forces every stage's cache off. Old raster
artifacts are not needed. If the original request lacked an appearance snapshot,
the command requires saved preparation fields and refuses to fetch today's
appearance as a substitute. Dry run does not submit anything.

The restart CLI needs no special deployed branch. Removal of the old resume
branch reaches AWS on the next operator-run `scripts/deploy_renderer.sh --yes`.
The collector logs `collect_components_profile`, and compose includes its
metadata fetch in `results_read_ms` for performance inspection.

A plain redrive of the original failing definition repeats the oversized results. Redrive
also retains the original state machine definition and reruns all Inline Map
iterations on DataLimitExceeded; a workflow-definition fix needs a new
execution. See [AWS redrive behavior](https://docs.aws.amazon.com/step-functions/latest/dg/redrive-executions.html).

For durable prevention while retaining Inline Map:

1. Continue storing complete per-component records in S3 (already done).
2. Make each Map iteration return only a tiny acknowledgement before the Map
   collects results; discard the aggregate output after synchronization.
3. Add a collector that checks the expected records and writes an S3 manifest
   or per-compose-batch manifests. Return only keys/counts to Step Functions.
4. Have each compose invocation read its required records from S3. Do not
   inject the entire component-results array into every compose input.

Reducing the current result to the seven composer fields would make this
specific job fit (~150 KB array), but is only an interim measure. Projecting
each task output must happen before Map aggregation, not in a later state.

For Distributed Map, exporting results through ResultWriter is another
option; it is not available for Inline Map. Both modes still need small
downstream payloads. See [AWS ResultWriter](https://docs.aws.amazon.com/step-functions/latest/dg/input-output-resultwriter.html).

Bounded state payloads do not remove Inline Map's separate execution-history
limit. Keep that constraint in mind for substantially larger task counts.
