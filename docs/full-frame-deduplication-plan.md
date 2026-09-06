# Full-frame deduplication plan

## Why this is the next optimization

The component compositor is currently dominated by WebP encoding rather than S3 or PNG work. For job `8773f7ad-b9c6-4a59-85fb-5e5cb2e8c278`, a ten-frame compose invocation averaged:

| Work | Average per invocation |
| --- | ---: |
| Read prepare manifest | 72.6 ms |
| Download component PNGs | 69.6 ms |
| Decode component PNGs | 30.4 ms |
| Compose RGBA frames | 136.9 ms |
| Encode WebP | 4,762.1 ms |
| Upload WebP | 476.8 ms |
| Total | 5,580.9 ms |

That job produced 120 logical frame objects, but only 30 unique WebP payloads. Ninety encoded frames were exact duplicates, including one payload repeated 89 times. Avoiding duplicate complete-frame composition and encoding is therefore a much larger win than changing the component PNG transport.

## Ranked follow-up ideas

| Rank | Idea | Expected win | Ease | Notes |
| ---: | --- | --- | --- | --- |
| 1 | Deduplicate complete frame recipes globally | Very high | Medium | The measured job could fall from 120 encodes to 30. |
| 2 | Run one unique composition per Lambda with Map concurrency 40 | High latency win | Easy after dedup | Gives up to 40 unique encodes one wave without multiplying duplicate work. |
| 3 | Lower the `cwebp` method | Medium | Easy | Trades compression efficiency or output size for speed. Benchmark before changing output defaults. |
| 4 | Enable `cwebp -mt` and allocate more CPU | Medium | Easy | Must be measured on Lambda ARM64; it may be less useful when each Lambda encodes only one frame. |
| 5 | Call libwebp directly with RGBA | Medium | Hard | Removes temporary PNG creation, filesystem I/O, and a subprocess, but encoding itself remains. |
| 6 | Merge adjacent identical animation frames by adding durations | Medium for some jobs | Medium | Reduces final WebP physical frames, but changes validation and timing semantics. Defer initially. |
| 7 | Store component RGBA as zstd instead of PNG | Low from current evidence | Hard | Download plus PNG decode was about 100 ms per ten frames, far below WebP encode time. |
| 8 | Overlap WebP uploads with encoding | Low after one-frame fan-out | Medium | There is no next encode to overlap in a one-composition invocation. |

## Exact deduplication key

A logical frame is represented by its ordered list of component raster identities. The identities already include every setting that affects a component's pixels, including renderer version, raster size, output size, viewbox, facing, weapon type, colors, layer name/index, transform matrix, darkening, state signature, and source-part digest.

The complete-frame key will be the exact ordered `layers` array from `component_frames`. Frame duration is deliberately excluded because it affects animation playback, not pixels. No perceptual hashing or image comparison is needed: equal ordered component identities under the same manifest canvas/settings produce equal composed pixels.

Deduplication is global across the job, not scoped to the existing ten-frame batches. This catches repeats on opposite sides of a batch boundary.

## Prepare-manifest additions

`PrepareFinish` will continue to emit `component_frames` for every logical animation frame. It will additionally emit `component_compositions`, in first-seen order:

```json
{
  "canonical_frame": 1,
  "layers": ["component-id-a", "component-id-b"],
  "logical_frames": [1, 5, 11]
}
```

`component_batches` will address ranges of `component_compositions`, rather than ranges of logical frame numbers. Initially the deployment default should be one unique composition per batch and Map concurrency 40. Both values remain environment-configurable.

The manifest change is additive. The compose worker will retain support for the old `frame_start`/`frame_end` batch contract so local tooling and in-flight manifests do not break during deployment.

## Compose behavior

For each unique composition assigned to an invocation, `ComposeComponentFrameChunk` will:

1. Download and decode the referenced component rasters once.
2. Compose the canonical frame once.
3. Encode and upload one WebP object, using the canonical logical frame number in its key.
4. Emit one `FrameRecord` for every logical frame represented by the composition.

Alias records share the same WebP key, SHA-256, byte length, and canvas, while retaining their own frame number and duration. Batch manifests may consequently contain non-contiguous logical frame numbers. `Finalize` already orders records by logical frame number and will continue requiring exactly the requested logical frame count.

Useful telemetry should distinguish:

- `unique_frames_encoded`
- `logical_frames_emitted`
- `deduplicated_frames`
- minimum and maximum logical frame represented by the invocation

## Final animation assembly

The first version will preserve every logical frame in the animated WebP, including its original duration. `Finalize` will download each distinct WebP key once and may pass the same local file to `webpmux` multiple times. This preserves current frame-count and duration semantics while eliminating repeated S3 downloads.

Merging consecutive equal frames into one physical WebP frame with a summed duration is a separate optimization. It should only be enabled after confirming player behavior, WebP duration limits, and any API consumers that expect a particular physical frame count.

## Correctness and failure handling

- Validate that every logical frame appears in exactly one composition group.
- Validate that each group's canonical frame is one of its logical frames.
- Validate that every grouped logical frame has exactly the group's ordered layer list.
- Reject out-of-range and duplicate logical frame numbers.
- Keep final duration validation against `frame_durations` unchanged.
- If two records reuse an S3 key, require their expected SHA-256 and byte length to match before downloading it once.
- A retried compose invocation writes the same canonical object key and batch manifest, preserving the existing idempotent behavior.

## Cost and latency expectation

Without deduplication, shrinking from ten logical frames to one frame per Lambda repeats manifests, downloads, cold starts, and initialization up to 120 times. It may reduce wall-clock latency while increasing aggregate GB-seconds.

With this job's measured deduplication, one unique composition per Lambda means about 30 invocations in a single concurrency-40 wave, rather than 12 ten-frame invocations that collectively perform 120 WebP encodes. Both aggregate work and critical-path latency should fall substantially. Jobs with no duplicates still retain the configurable batch size as a cost/latency control.

## Tests and rollout

1. Unit-test recipe grouping, including duplicates across a former batch boundary and equal pixels with different durations.
2. Unit-test new and legacy batch selection in the compose worker.
3. Verify one encoded/uploaded object produces multiple logical `FrameRecord`s with preserved durations.
4. Unit-test finalizer download deduplication and reject conflicting metadata for a reused key.
5. Run Rust formatting, checks, and tests for the pipeline, compose, and finalizer crates, followed by CDK tests/synthesis.
6. Deploy manually to the test stack with ARM64 images and admissions left enabled.
7. Submit equivalent forced-no-cache jobs, compare unique recipes, encode count, aggregate Lambda GB-seconds, compose wall time, final frame count/durations, and rendered output hashes.

## Out of scope for this change

- Component-raster RGBA/zstd transport
- WebP quality or method changes
- `cwebp -mt` or memory changes
- Direct libwebp integration
- Adjacent-frame duration merging
- Deployment automation or running a deployment
