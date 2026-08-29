# AQW character rendering pipeline speed review

## Scope

This review uses an isolated clone of `aqw-char-rendering-infra` at commit
`8be4390fdde95cceaaec6f03ce2a32d5e9a394e7` (committed 2026-08-29 10:17 PDT).
The active checkout was not read after the clone was made and was not modified.

The reviewed workflow is:

1. `PrepareResolve`
2. `ExportSourceFrames` Map, one Lambda per source SWF
3. `PrepareFinish`
4. `ProbeFrameBounds` Map
5. `FitSharedCanvas`
6. `RenderFrameBatches` Map
7. `FinalizeAnimation`

## Short answer about knowing the loop length first

Yes: for the immutable AQW asset corpus, the normal path should know the loop
length before request-time SVG generation. The best design is to analyze each
SWF once when the dataset is built, store a content-addressed animation
manifest, and look up the selected symbols after the character's appearance is
resolved.

Reading only a requested symbol's declared root frame count is not sufficient.
The selected root frame can hold independently advancing nested sprites. A
correct analyzer must traverse the display-list graph reachable from the
selected Ready/Idle frame and determine the schedule of those nested sprites.
Adobe's SWF specification says a `DefineSprite` has its own frame count and can
play independently, be nested, and be controlled by actions. See the
[SWF v19 specification, Chapter 13](https://open-flash.github.io/mirrors/swf-spec-19.pdf).

The local Soltina assets demonstrate both points:

| Source | Requested/root timeline | Relevant nested timeline |
| --- | ---: | ---: |
| `RePanopticonProisoPhormo.swf` | armor symbols are mostly 1 frame | 87-frame head blink |
| `SparklyMoonWandCC.swf` | 1 frame | 60 frames |
| `EmpressWings.swf` | 37 frames; selected Idle starts at frame 1 | 72 frames |
| `PanopticonPhormoVisage.swf` | 1 frame | 87-frame face/blink schedule |

For the current blink policy, the repeating item period is therefore:

```text
item_period = lcm(60, 72) = 360
blink_span = 87
output_frames = ceil(blink_span / item_period) * item_period = 360
```

That result can be obtained from SWF structure before exporting 360 SVG states.
It agrees with the known Soltina result. The 87-frame eye animation should be a
one-shot span that freezes on its last frame; it must not be included in the
LCM as a repeating driver.

There are three reasons to retain a validated fallback rather than trusting a
simple LCM of every declared frame count:

- Only timelines reachable from the selected root frame matter. Unused library
  sprites must not contribute to the result.
- Placement/removal, phase offsets, `stop`, `play`, and `goto*` actions can
  change which timeline is active and how it advances. Arbitrary ActionScript
  makes the exact Flash-runtime loop harder than structural inspection alone.
- A declared 72-frame timeline can have a shorter *visual* period because some
  or all frames are identical. Structural LCM is a safe candidate, but not
  always the shortest output matching the current SVG-hash behavior.

For this FFDec-only renderer, exact Flash-runtime behavior is not required if
FFDec itself does not execute those actions during subsprite export. The target
should be explicitly defined as “the period of the pinned FFDec export.” Analyze
that once per immutable SWF, cache it, and use structural analysis plus a
one-time exported-signature validation to cover unusual assets.

## Why knowing the number does not automatically split FFDec export by frame

FFDec 26.2.1 exposes `-sublength <length>`, which exports the first N nested
subframes. It does not expose a nested-subframe start/range option. Its
`-select` ranges choose frames of the selected root character, which is a
different axis. The current one-Lambda-per-source parallelization is therefore
the useful stock-FFDec boundary. This matches the
[FFDec CLI documentation](https://github.com/jindrapetrik/jpexs-decompiler/wiki/Commandline-arguments),
and FFDec's own description notes that subsprite animations may not have one
obvious endpoint because each nested sprite has its own timeline
([FFDec 24 subsprite export notes](https://blog.free-decompiler.com/2025/06/24/news-in-ffdec-24-0-x/)).

Once the total is known, composition/raster work can be split immediately and
evenly. Splitting one FFDec source export by nested frame would require one of:

- pre-exporting and caching the source's vector states, which is recommended;
- adding a true nested-subframe range to FFDec/JPEXS;
- running multiple prefix exports and discarding repeated prefixes, which is
  parallel but wastes enough work that it is usually a poor trade.

## Highest-priority findings in the current pipeline

### 1. Raster workers download part archives they never use

`stages/render.py` downloads and extracts every requested part archive before
branching on `mode`. In `mode="raster"`, it then downloads the already composed
SVGs and never calls `compose_frame`, so all part-archive GETs, decompression,
and local writes in that wave are unnecessary.

This is the safest first optimization: perform part download/extraction only in
probe mode. It changes no output.

### 2. Four-frame batches amplify overlap and S3 overhead

At 360 frames and batch size 4 there are 90 batches. Every non-first batch uses
one predecessor frame, so each Map processes 449 frame positions rather than
360: 89 extra, or 24.7% overhead.

The predecessor is needed during raster/delta encoding, but the probe Map does
not need to recompute it. The entire probe Map is a barrier before raster begins,
so the preceding SVG has already been uploaded by the preceding probe batch.
Starting probe batches at `frame_start` would remove 89 compositions, probes,
and duplicate SVG PUTs: about 19.8% of the current 449-frame probe workload.

The current per-symbol chunk format also produces many tiny operations. With
roughly 15 symbols, a non-first four-frame worker commonly downloads both the
previous and current chunk for every symbol. That can approach 30 part GETs per
worker, per Map, before blink-freeze chunks. At a 2,000-frame cap, the exporter
can create approximately 500 chunks *per symbol*.

Prefer one archive per `(source SWF, chunk ordinal)` containing all selected
symbols for that source. Optionally include the one predecessor frame in the
chunk. This reduces the normal worker from roughly one or two GETs per symbol
to one GET per source and trades a small amount of duplicate storage for much
lower request latency.

### 3. Preparation stores the exported corpus twice

Each export Lambda currently creates all per-symbol chunk archives and a full
archive containing the same frames. `PrepareFinish` downloads and extracts all
full archives, while workers later download chunks. In addition:

- chunks are generated for `max_frames + validation_frames`, even when the
  final animation is much shorter;
- loop detection hashes the same SVG bytes repeatedly in
  `loop_driver_exports`, item-loop detection, blink detection, and diagnostic
  `symbol_loop_info`;
- `shared_viewbox` reads the frame headers again;
- `archive_bytes` omits the full-archive size, so logs under-report output.

As an intermediate improvement, compute each frame signature, state ID, period
candidate, and vector-header bounds while the raw exports are already local in
`prepare_export_source`. Upload a small metadata object and remove the full
archive/finish reconstruction path. This still discovers the period after an
FFDec export, but eliminates a large duplicate I/O phase while the offline
manifest is being built.

### 4. The exported validation tail is currently discarded by detection

Resolve requests `max_frames + 8`, but Finish constructs
`detection_exports` by slicing every list back to `max_frames`. The detector's
comment says the trailing states validate periods at the cap, but those states
never reach it. With a 360-frame cap and an eight-frame validation requirement,
periods 353–360 cannot be proven by the current call.

Either pass the validation tail into detection or stop exporting it. The former
is the intended correctness fix; a precomputed manifest eventually removes
this request-time scan entirely.

### 5. The probe wave is expensive and incompletely measured

For a 2048 output, every frame is alpha-probed at `max(1024, max_size * 2)`, or
4096 pixels on the dominant dimension, before it is rasterized again at 2048.
The code describes this as a low-resolution probe, but it is higher resolution
than the final render. Pixel work grows roughly with area, so a 4096 probe has
about 16 times the pixels of a 1024 probe.

First benchmark a fixed 512 or 1024 probe with one or two pixels of conservative
padding mapped back into SVG space. Better, store visible bounds for every
cached unique vector state and union the transformed bounds at request time.
That can remove the entire Probe Map and Fit Lambda while keeping one shared
canvas.

Do not revert blindly to FFDec's broad SVG page bounds—the repository history
shows those made Artix occupy only about 45% of the canvas width. Validate any
precomputed-bound approach against a fixture set containing glow/filter,
outlier, mirrored, and highly animated items.

### 6. Resolve and Finalize still contain easy serial I/O

Resolve downloads `item_db.json` even when no item override is present. Make
that download conditional. Its independent character asset downloads are also
serial and can use a small thread pool. A checksum-keyed warm `/tmp` cache is a
reasonable second step for reused Lambda environments.

Finalize downloads every encoded WebP frame serially—360 GETs for Soltina and
up to 2,000 at the configured contract ceiling. Use bounded parallel downloads,
or have each render batch upload one archive containing its frame fragments and
manifest. The latter reduces the finalizer to one GET per batch.

## Recommended animation manifest

Extend dataset construction to create a sidecar keyed by immutable content,
not by item name:

```text
animation-metadata/<schema>/<ffdec-version>/<swf-sha256>.json
vector-states/<schema>/<ffdec-version>/<zoom>/<swf-sha256>/<symbol>/<root-frame>.tar.zst
```

For each exported symbol/root-frame pair, store at least:

- SWF SHA-256, character ID/class, selected root frame, and analyzer version;
- reachable structural period and exact validated visual period;
- a compact frame-to-state schedule and hashes of unique states;
- start phase or non-repeating prelude, if present;
- whether it is the standard blink schedule;
- visible bounds per unique state;
- an `exact`, `validated`, or `runtime_fallback` classification;
- color rules and authored placement transforms, which are already pure
  functions of the SWF bytes.

The analyzer should build each `DefineSprite` timeline's display-list states
from Place/Move/Remove/ShowFrame tags, traverse only children reachable from the
selected root frame, and use cycle detection on the complete active state. It
should recognize common timeline-control actions where practical. Assets with
unsupported dynamic behavior should use the existing export-and-hash fallback,
then cache that result by content hash.

For corpus assets this work happens once during dataset creation. Official
fallback assets can be analyzed once on first use and written to the same
content-addressed cache. A request then becomes:

1. Resolve FlashVars and selected source hashes.
2. Look up the selected symbol manifests.
3. Remove the standard blink schedule from repeat drivers.
4. Compute the LCM, align the one-shot blink, and apply `max_frames`.
5. Partition frames before any request-time vector work.
6. Fetch only the unique vector states required by each batch.

Precomputing loop metadata is useful by itself, but precomputing the raw vector
states is the larger win: it removes Java/FFDec startup and export from normal
requests. User colors and character assembly remain request-time operations;
the cached raw SVG states are appearance-independent.

## Batching recommendation

“Even frame counts” are not always even work. SVG complexity, filters, output
area, and WebP delta size can vary by frame. Keep ranges contiguous for delta
encoding, but use a cost estimate such as cached SVG bytes, path count, filter
count, or prior raster timing to divide the timeline into ranges with similar
predicted cost.

Also avoid a fixed batch size of 4 for every job. Pick a target number of
workers based on frame count and the concurrency budget, then divide using
quotient/remainder so batch sizes differ by at most one before cost weighting.
Tune so fixed costs—cold start, S3, archive extraction, and the predecessor
frame—are no more than roughly 10–15% of batch time. Cap per-job concurrency so
one 2,000-frame render cannot consume the whole regional pool.

## Measurement fixes before comparing designs

The current historical evidence is useful but predates parts of the v6
workflow:

- commit `1cced3e` reports Prepare improving from 37 s to 23 s;
- commit `55224f6` reports the 360-frame Map improving from 181.7 s to 31.5 s
  with resvg/high concurrency, and total execution from 237.6 s to 93.6 s;
- the later probe-fit-raster wave and parallel Prepare need a new end-to-end
  baseline.

Fix the timing fields first. In `PrepareFinish`, `archive_download_ms` currently
includes extraction, while `archive_extract_ms` mostly measures file discovery
and manifest construction. Probe raster time and probe SVG upload time are not
named and fall into `unaccounted_ms`. ExportSource and Finalize also need phase
breakdowns.

For Artix, Soltina, a static character, and a 2,000-frame/capped stress fixture,
record cold and warm p50/p95 values for:

- Step Functions wall time for every state and Map;
- Lambda billed duration, memory, init duration, and throttles;
- FFDec/JVM startup and export;
- archive object count, bytes, creation, upload, GET, and extraction;
- signature/period analysis;
- compose, probe, raster, cwebp, frame upload;
- final frame downloads, webpmux, validation, and publication;
- total GB-seconds and S3/Step Functions request counts per render.

## Suggested implementation order

1. Remove unused raster part downloads, probe overlap recomposition, and the
   unconditional item database download. Fix validation-tail use and timing.
2. Change chunks from per-symbol to per-source bundles; parallelize/bundle
   Finalize downloads.
3. Compute signatures and bounds once in ExportSource and remove full duplicate
   archives from PrepareFinish.
4. Add the dataset animation manifest so frame count is known immediately after
   appearance resolution and only the needed frame range is exported.
5. Add content-addressed unique vector-state bundles and remove request-time
   FFDec on cache hits.
6. Validate precomputed visible bounds; if they hold, remove the runtime Probe
   Map and Fit Lambda.
7. Tune weighted batch sizes, per-job concurrency, and Lambda memory only after
   the preceding duplicate work is gone.

The architectural target is not “more Lambdas for the same repeated work.” It
is “analyze and export each immutable SWF state once, then distribute only
character-specific composition, rasterization, and encoding.”
