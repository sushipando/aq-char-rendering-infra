# Component-Raster Character Rendering Pipeline

Status: implemented in development. Unique component states are rasterized at
the requested high-resolution raster size, downsampled once onto the exact
output pixel grid, then composed and encoded in a second Map. The existing
finalizer performs the animation mux and publish.

## Goal

Stop rasterizing every component's SVG filters again inside every complete
character frame.

The current renderer composes a complete SVG for each output frame and then
rasterizes that SVG. This repeats expensive work when a component state is
unchanged across many output frames. For example, a static skateboard with 17
three-pass glow filters causes 51 Gaussian-blur operations in every complete
frame. In an 87-frame animation, the same static artwork therefore causes
4,437 blur operations.

The proposed pipeline rasterizes each unique, fully placed component state
once. Complete frames are then assembled from cropped transparent PNG layers
using ordinary alpha compositing.

The initial 25-frame single-compositor benchmark established correctness and
made each cost visible. The current implementation supports the configured
120-frame cap and divides final-frame work into configurable chunks.

## Current architecture

```text
Discord request
  -> PrepareResolve
  -> ExportSourceFrames Map
       FFDec: SWF -> unique SVG states
  -> PrepareFinish
       choose at most the configured output-frame cap
       determine the shared character viewbox and pixel scale
       build unique placed-component raster tasks
  -> RasterComponentStatesInline | RasterComponentStatesDistributed Map
       one Lambda invocation per unique placed component state
       component SVG + character placement + colors -> high-resolution PNG
       premultiplied Lanczos -> cropped output-grid PNG + integer offset
  -> ComposeComponentFrameChunks Map
       each Lambda builds a configurable contiguous chunk of frames
       alpha-composite output-size PNG layers at integer offsets
       encode each frame as WebP
       upload independent encoded frames plus a compact batch manifest
  -> FinalizeAnimation
       concurrently download encoded frames
       mux, validate, publish, and complete the job
  -> complete job and send result
```

Inline mode invokes the component-raster Lambda directly; distributed mode
invokes it through Express child workflows. Either Map is the synchronization
barrier: the compose Map starts only after every component iteration succeeds.
SQS is not used between these stages; an empty queue would not prove that every
worker completed successfully.

## Unit of component work

A raster task represents one unique **placed component state**, not merely one
source SVG and not necessarily one output timeline position.

Its identity must include all inputs that can change its pixels:

```text
raw SVG state hash
+ semantic placement/layer name
+ complete characterB placement matrix
+ front/back darkening behavior
+ character-facing behavior
+ user color signature
+ shared viewbox/pixel scale
+ raster size
+ renderer version
```

Examples:

- A static weapon used in 25 output frames creates one weapon raster task.
- If armor frames 4 and 5 contain byte-identical visual states, they share one
  raster task.
- The same hand source requires separate front-hand and back-hand tasks because
  their transforms and darkening differ.
- A dagger can require separate primary-hand and off-hand tasks.
- A static component with different user colors cannot reuse a raster created
  for another color signature unless it is later decomposed into reusable tint
  masks.

Tasks must use deterministic IDs and deterministic S3 keys so retries are
idempotent.

## Component rasterization

Each component worker will:

1. Download the requested raw SVG state and the small job/component manifest.
2. Import the SVG with the existing FFDec registration corrections.
3. Apply existing scripted color rules and authored color transforms.
4. Apply the complete characterB placement, facing, display scale, and
   front/back darkening while the component is still vector artwork.
5. Use the job's already-decided shared character viewbox and raster size to
   preserve exactly the same pixels-per-character-unit as the final frame.
6. Rasterize once at the unchanged high-resolution raster size. Existing SVG
   minimum-stroke calibration happens here and is not recalculated at the
   smaller output size.
7. Crop transparent padding, then shrink the finished RGBA pixels with
   premultiplied-alpha Lanczos filtering. The crop uses the full character's
   exact resampling phase, including odd dimensions such as 697 -> 348.
8. Crop the resized transparent padding and record the PNG's integer `x` and
   `y` position on the shared output canvas, plus width, height, checksum, and
   S3 key.
9. Upload the smaller PNG and a compact result record.

No affine transform is applied to a PNG. All scale, rotation, mirroring, skew,
and translation are baked into the SVG before its single rasterization. The
only PNG transform is the common raster-to-output shrink. Final frame
composition only places already transformed and resized PNGs at integer
offsets.

The component SVG should use a tight page at the exact job pixel scale rather
than rasterizing a full transparent 2048- or 4096-pixel character canvas for
every small component. Its page origin must be derived from the common canvas
so the returned crop offset remains exact.

## Shared viewbox requirement

Every component raster must use the same mapping from character-space units to
output pixels. `PrepareFinish` must therefore determine the shared animation
viewbox before the component-raster Map begins.

The first implementation should reuse the current precomputed/alpha-probed
unique-state bounds and existing shared-viewbox logic. It must not independently
fit each component to its own PNG, because that would give different components
different scales and make integer-offset composition impossible.

## Component-raster manifest

`PrepareFinish` should write a manifest containing:

```json
{
  "job_id": "...",
  "frame_count": 25,
  "raster_size": 2048,
  "output_size": 1024,
  "component_raster_space": "output",
  "viewbox": [0, 0, 100, 140],
  "component_tasks": [
    {
      "task_id": "...",
      "symbol_key": "weapon",
      "source_state": 1,
      "placement": "weapon",
      "layer_index": 19,
      "svg_key": "jobs/.../states/weapon/000001.svg"
    }
  ],
  "frames": [
    {
      "number": 1,
      "duration_ms": 42,
      "layers": ["component-task-id-1", "component-task-id-2"]
    }
  ]
}
```

The exact schema can differ, but layer order must be explicit and stable.
Frame records should reference task IDs instead of repeating S3 paths and
placement metadata.

Each successful component task should produce a record similar to:

```json
{
  "task_id": "...",
  "png_key": "jobs/.../component-rasters/...png",
  "x": 314,
  "y": 108,
  "width": 721,
  "height": 1334,
  "sha256": "...",
  "rasterize_ms": 1234.5
}
```

## Parallel frame composition

`PrepareFinish` globally groups logical frames with the same exact ordered
component-task IDs. It divides the unique recipes into consecutive batches of
`ceil(unique recipes / compose concurrency)`. Development uses Map concurrency
40: up to 40 recipes use one frame per invocation, while 120 unique recipes
become 40 three-frame invocations.

It will:

1. Read the component results and validate every task referenced by its chunk.
2. Download only the unique component PNGs needed by that chunk, concurrently
   with a bounded worker pool.
3. Decode each needed PNG once and retain it for the invocation.
4. For each unique composition, allocate one transparent output-size RGBA canvas.
5. Alpha-composite the referenced layers in exact back-to-front order at their
   recorded integer offsets.
6. Encode each complete frame with the configured WebP settings, uploading
   the preceding encoded frame concurrently when the batch has multiple frames.
7. Upload each unique frame, then emit a logical frame record for every frame
   sharing that recipe while preserving each duration.
8. Let `FinalizeAnimation` download each unique object once, gather all
   logical frame records, mux,
   validate, publish, and complete the job.

Pillow remains the compositor because the operation is cropped RGBA
source-over with no runtime affine transformation. A production-artifact
benchmark found output-grid Pillow substantially faster than ImageMagick.
Premultiplied-alpha resizing now happens once per unique component state in
the raster Map; it is no longer repeated for every completed frame. Manifests
created by older deployments continue to use the raster-grid/full-frame
downsample fallback.

## Step Functions shape

```text
PrepareResolve
  -> ExportSourceFrames Map
  -> PrepareFinish
  -> RasterComponentStatesInline | RasterComponentStatesDistributed Map
  -> ComposeComponentFrameChunks Map
  -> FinalizeAnimation
```

Each request explicitly selects `component_raster_mode` as `inline` or
`distributed`, with `inline` as the default. Inline directly invokes up to 40
component Lambdas concurrently and processes remaining tasks in later waves.
Distributed uses Express child workflows and `componentRasterConcurrency`; the
development value of 200 allows up to 200 unique component states in one wave.
The state machine never switches modes based on task count. Lambda regional
concurrency is still the account-wide safety ceiling.

Each iteration needs normal Lambda service/throttle retries. A failed
component must fail the Map and enter the existing protected workflow failure
handler; the compositor must never silently omit a failed layer.

`ComposeComponentFrameChunks` replaced the legacy `RenderFrameBatches` Map;
the full-frame legacy renderer has since been removed and every job now runs
`RasterComponentStatesInline|RasterComponentStatesDistributed ->
ComposeComponentFrameChunks -> FinalizeAnimation`.
`FinalizeAnimation` remains the lightweight mux and publish barrier. Keeping
composition distinct from component rasterization makes timings, memory use,
and concurrency independently tunable.

## Benchmark configuration

Add explicit development tuning for the experiment:

```text
componentRasterInlineConcurrency = 40
componentRasterConcurrency = 200
componentRasterFrameCap = 120
componentComposeConcurrency = 40
```

The existing request's raster size, output size, zoom, padding, WebP quality,
and WebP method remain authoritative.

The component path is mandatory; there is no legacy fallback renderer.

### Concurrency 5 -> 40 deployment benchmark (2026-09-02)

The same 30-frame Alina request produced 134 unique component tasks before
and after the tuning-only deployment:

- concurrency 5: `RasterComponentStates` took 11.921 seconds; the complete
  Step Functions execution took 34.495 seconds.
- concurrency 40: `RasterComponentStates` took 3.852 seconds; the complete
  Step Functions execution took 15.036 seconds.

The raster Map was 3.1x faster. The complete workflow was 2.3x faster, though
non-raster stages also had normal cold-start and service-latency variance.

### Inline 40 -> Distributed 200 deployment benchmark (2026-09-02)

The 30-frame, 256px Alina request still produced 134 component tasks and the
same 338,472-byte animation after changing `RasterComponentStates` to a
Distributed Map with Express children:

- Inline concurrency 40: the raster Map took 3.852 seconds. Component Lambda
  completions spanned 2.596 seconds as the work ran in four waves.
- Distributed concurrency 200, warm run: the raster Map took 6.960 seconds.
  All 134 children succeeded with no redrives, and component Lambda completions
  spanned only 0.254 seconds.

The higher concurrency dispatched the raster work as intended, but AWS spent
about 5.7 seconds after the final Lambda completion closing and aggregating the
Map Run. Consequently, Distributed Map is slower for this small-raster case
despite much tighter compute fan-out. The warm complete workflow took 13.722
seconds, but that is not an isolated raster comparison because the other stages
also varied.

A 30-frame Alina validation at the default 2048px output also succeeded with
134/134 child executions and no redrives. `RasterComponentStates` took 6.840
seconds; individual raster calls had a 1.119-second median and their completion
times spanned 3.003 seconds. The complete workflow took 23.215 seconds and
produced the expected 4,168,612-byte animation.

## Measurements

### Alina 120-frame composition benchmark (2026-09-01)

The exact component artifacts from the successful 4096 -> 2048 Alina job
were downloaded and replayed locally:

- Current Pillow composition plus full-frame downsampling: 16.05 seconds
  (4.34 seconds composition, 11.72 seconds downsampling), excluding WebP.
- Grid-aligned component downsampling plus output-size Pillow composition:
  1.82 seconds including the one-time 136-component resize, or 8.8x faster.
- ImageMagick output-size composition: about 0.151 seconds per frame, versus
  0.0085 seconds per frame for Pillow.
- Twelve parallel 10-frame workers using the unchanged production pixel path,
  including WebP encoding and final mux: 15.81 seconds locally.
- All 120 decoded output frames had zero premultiplied-RGBA difference from
  the deployed v18 animation.

The output-grid experiment had normalized premultiplied-RGBA MAE 0.00082
against full-frame downsampling, with a few localized larger edge differences.
Follow-up Muq and Soltina comparisons at both 512 and 2048 output sizes found
the normal renders effectively indistinguishable. Muq's 512 test changed only
2.45% of pixels above 1/255; Soltina's translucent-cape test changed 3.45%,
with only 145 pixels above 16/255. The optimized phase-aware tight-crop
implementation reproduced the approved full-canvas component reference
exactly at 2048 and within one channel value at one pixel at 512.

### Pillow versus pyvips Lambda benchmark (2026-09-01)

Both backends were tested sequentially through the deployed 3008 MiB x86_64
`ComponentCompose` Lambda using the same saved Alina artifacts. Direct
synchronous invocation kept effective Lambda concurrency at one without
changing the production Map limit.

- One 120-frame warm invocation: Pillow 136.72 seconds; pyvips 129.63 seconds.
  pyvips was 7.09 seconds (5.2%) faster overall.
- Three warm 10-frame invocations, matching the production chunk size:
  Pillow averaged 11.984 seconds; pyvips averaged 11.561 seconds. pyvips was
  0.422 seconds (3.5%) faster.
- At 10 frames, pyvips composition and downsampling were slower than Pillow;
  its small net win came from the temporary-PNG/WebP encoding phase.
- The first lazy pyvips initialization added about 3 seconds. Because this
  service is expected to be used infrequently and workers may often be cold,
  that startup cost can outweigh the warm 0.42-second chunk advantage.
- The decoded output was visually indistinguishable in the inspected Alina
  frame but not pixel-identical (normalized premultiplied-RGBA MAE about
  0.00093 versus Pillow).

The deployed default therefore remains Pillow. pyvips is retained only as an
explicit benchmark backend until a larger multi-character fidelity and cold-
start sample justifies changing the default.

Record at least these timings and sizes:

### Preparation

- SWF download and FFDec SVG export time
- unique raw SVG state count
- unique placed component task count
- manifest construction time

### Component-raster Map

- Map wall time
- per-task download, SVG import/customization, SVG write, rasterization, crop,
  and upload time
- input SVG bytes and output PNG bytes
- filter count and SVG byte size for identifying expensive tasks
- Lambda duration, billed duration, memory, init duration, and throttles

### Single final-frame Lambda

- component manifest download time
- concurrent PNG download time and bytes
- PNG decode time
- alpha-composition time for each frame and total
- downsample time
- WebP encode time
- mux time
- upload time
- peak memory if available
- total Lambda duration and billed duration

### End to end

- Step Functions execution duration
- total Lambda GB-seconds
- total Lambda invocation count
- S3 GET/PUT count and bytes
- output file size

The primary comparison is against the legacy complete-SVG pipeline for the
same username, appearance, facing, raster/output sizes, WebP settings, and
output frames.

## Correctness fixtures

At minimum compare these cases before enabling the path generally:

- Muq: static, filter-heavy skateboard; proves expensive component reuse.
- Fiy: scripted hair colors and multiply-blended shading.
- Try: animated pet marker placement and nested animation.
- Queen Annie 012: cape pauses plus a continuing nested bird animation.
- Artix: skin/armor colors, hidden-helm hair behavior, and body-layer order.
- A dagger or gauntlet user: repeated source art with different placements.

Compare frame count, durations, canvas dimensions, alpha bounds, layer order,
colors, and representative pixel differences. The new path should not be
judged only by whether the animation looks roughly similar.

## Expected result

This architecture does not make an intrinsically expensive SVG state free.
It changes how often that state is rasterized.

For a static filter-heavy component used in 120 frames:

```text
legacy: 120 expensive component rasterizations
component path: 1 expensive component rasterization + 120 cheap PNG composites
```

The staged benchmarks established separately:

1. How much wall time is saved by rasterizing unique component states once.
2. Whether downloading and composing component PNGs is actually negligible.
3. How much S3 storage and request overhead the intermediate rasters add.
4. That a single Lambda can compose 120 frames but sequential WebP encoding
   and full-frame downsampling dominate its wall time.
5. That splitting the existing pixel path into twelve 10-frame workers
   removes the serial bottleneck.
6. That moving the shrink to unique component states removes repeated
   full-frame downsampling with an accepted, visually indistinguishable edge
   difference at translucent overlaps.

## Deferred work

The current implementation deliberately does not include:

- SQS component-task buffering or task-token callbacks;
- permanent cross-job component-raster caching;
- reusable color-region masks;
- static-band precomposition;
- a native Rust, Skia, or libvips compositor;
- automatic cost-weighted component scheduling.

These remain follow-up optimizations. Output-grid component downsampling is
implemented; its small edge differences are the expected result of resampling
and source-over composition not commuting at translucent overlaps.
