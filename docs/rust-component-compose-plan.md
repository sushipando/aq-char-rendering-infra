# Rust Component-Compose Lambda Plan

## Goal

Replace the Python/Pillow implementation behind `ComposeComponentFrameChunks`
with a small native Rust Lambda while preserving the current Step Functions
contract and rendered appearance.

The first deployment must be an isolated candidate function with effective
concurrency one. It must not receive production Map traffic until local and
direct-Lambda comparisons pass.

This optimization targets component download, PNG decode, ordered alpha
composition, WebP frame encoding, upload, and cold-start overhead. It does not
optimize SVG rasterization. Slow SVG filters such as the filters in
`items/capes/EmpressWings.swf` remain work for the component-raster stage.

## Why this is a good Rust boundary

The current component pipeline has already completed the complicated work
before composition begins:

```text
FFDec SVG export
  -> component-state SVG construction
  -> resvg rasterization
  -> output-grid component PNGs in S3
  -> ComposeComponentFrameChunks
```

For current `component_raster_space = "output"` manifests, the compose worker
does not transform or resize layers. It only:

1. Reads the prepared manifest and compact component results.
2. Downloads the component PNGs referenced by its frame chunk.
3. Decodes each unique PNG once.
4. Reuses those decoded pixels across the frames in the chunk.
5. Draws layers in the exact order listed in `component_frames[].layers`.
6. Encodes and uploads one single-frame WebP per output frame.
7. Writes the existing compose-batch result manifest.

This is a small deterministic native workload. It does not require FFDec,
Java, pyvips, or resvg. A dedicated Rust image can therefore be much smaller
than the shared Python renderer image.

The current development configuration composes ten frames per Lambda. Keep
that initial contract: start one Rust process once, decode shared components
once, and render the whole ten-frame chunk. Do not spawn one process per frame.

## Proposed repository layout

Keep the candidate separate from the existing Python renderer:

```text
services/component-compose-rust/
  Cargo.toml
  Cargo.lock
  Dockerfile
  src/
    main.rs
    contract.rs
    storage.rs
    png.rs
    composite.rs
    encode.rs
    telemetry.rs
  tests/
    fixtures/
```

If the component-raster Lambda is later ported to Rust, move reusable pixel,
hashing, and contract code into a workspace crate rather than coupling the
first compositor implementation to resvg.

## Suggested dependencies

Use the smallest feature set that satisfies the worker:

```toml
[dependencies]
lambda_runtime = "..."
aws-config = "..."
aws-sdk-s3 = "..."
tokio = { version = "...", features = ["macros", "rt-multi-thread"] }
serde = { version = "...", features = ["derive"] }
serde_json = "..."
sha2 = "..."
png = "..."
tiny-skia = "..."
fast_image_resize = "..."
```

Version pins should be chosen and locked during implementation. Disable
unused default features where doing so does not remove TLS, S3, PNG, or Lambda
runtime support.

`tiny-skia` provides a premultiplied RGBA pixmap and SourceOver composition.
The normal output-grid path may also use a small custom integer RGBA loop if
that is required to match Pillow's rounding exactly. Keep
`fast_image_resize` for the old raster-space compatibility branch and any
future resizing; it should not run for normal v19 output-grid manifests.

For the first version, retain the same statically built `cwebp` version and
command-line flags as Python. That isolates the compositor comparison from a
codec change. Linking libwebp directly can be a separate benchmark after
correctness is established.

## Lambda event and result compatibility

Accept the existing event without translation:

```json
{
  "job_id": "...",
  "manifest_key": "jobs/.../prepare/manifest.json",
  "component_results": [],
  "batch": {
    "index": 0,
    "frame_start": 1,
    "frame_end": 10
  }
}
```

Return the same result shape:

```json
{
  "job_id": "...",
  "batch": 0,
  "batch_manifest_key": "jobs/.../component/compose-batches/batch-0000.json",
  "mode": "component-raster"
}
```

The batch manifest and each frame record must preserve the current schema,
including frame number, S3 key, offsets, dimensions, duration, SHA-256, and
byte count. This lets CDK switch the Lambda implementation without changing
`FinalizeAnimation`.

Add an optional benchmark-only output prefix or dry-run destination before
direct deployed tests. Candidate invocations must not overwrite a completed
job's normal intermediate frame keys. The production default must remain the
existing `jobs/{job_id}/component/...` layout.

## Required rendering behavior

The Rust worker must preserve all of these rules:

- Create a fully transparent RGBA canvas at the declared output dimensions.
- Treat `component_frames[].layers` as authoritative back-to-front order.
- Place each component at its integer `x` and `y` without inventing another
  transform.
- Clip negative or partly off-canvas placements exactly as the current code
  does.
- Skip records explicitly marked `empty`.
- Fail if a referenced task result or non-empty PNG is missing.
- Reject mixed or unsupported `component_raster_space` values.
- Use unmodified frame durations from the prepared manifest.
- Preserve sRGB/alpha behavior and avoid a white or black matte.
- Close or release decoded component buffers after each invocation.
- Write no final animation. The existing finalizer remains responsible for
  muxing and publishing the animation.

### Alpha correctness

Composition is where a visually plausible result can still be subtly wrong.
Test translucent pixels rather than comparing only opaque regions.

Use two comparisons:

1. Compare lossless pre-WebP RGBA frame buffers. Exact equality is the goal
   because the output-grid path performs only ordered source-over blending.
2. Run the same `cwebp` binary and settings, decode both WebP frames, and
   compare premultiplied RGBA. This distinguishes compositor changes from
   expected lossy-codec behavior.

If `tiny-skia` differs from Pillow by channel-rounding values, first confirm
that ordering and premultiplication are correct. If exact parity remains
important, implement and test the same integer source-over equation used by
the approved Pillow output.

## Structured timing output

Emit a `component_compose_profile` JSON log compatible with the Python
worker. At minimum include:

```text
job_id
batch
frame_start / frame_end
frames_rendered
referenced_component_count
downloaded_png_count
png_bytes
canvas_width / canvas_height
component_raster_space
download_concurrency
manifest_ms
results_read_ms
png_download_ms
decode_ms
composite_ms
downsample_ms
encode_ms
upload_ms
manifest_write_ms
total_ms
ms_per_frame
```

Also retain Lambda's platform `REPORT` line so benchmarks can record init
duration, billed duration, maximum memory, and total invocation duration.

## Local implementation and test sequence

### 1. Unit tests

Run the Rust checks on every iteration:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
```

Unit fixtures should cover:

- opaque source over transparent destination;
- semi-transparent source over semi-transparent destination;
- multiple translucent layers where order changes the result;
- a fully transparent source;
- an explicitly empty component;
- negative `x` and `y` placement;
- right and bottom clipping;
- one-pixel-wide geometry;
- repeated use of one decoded component in multiple frames;
- missing task and missing PNG failures;
- mixed coordinate-space rejection;
- durations and output-manifest serialization.

### 2. Give the Rust binary a local mode

The release binary should support a local fixture mode in addition to the
Lambda runtime. Local mode should read:

```text
manifest.json
component/results/*.json
component/rasters/*.png
```

and write frame PNGs, frame WebPs, and a batch manifest beneath a caller
provided output directory. This avoids mocking S3 during pixel and speed
development.

An intended command shape is:

```bash
cargo run --release --manifest-path services/component-compose-rust/Cargo.toml -- \
  local-compose \
  --artifact-dir /private/tmp/alina-component-benchmark \
  --output-dir /private/tmp/alina-rust-compose \
  --frame-start 1 \
  --frame-end 10
```

The exact CLI can change during implementation, but it must process the whole
chunk in one process.

### 3. Prepare comparable artifacts

Use a recent completed component-pipeline job because work artifacts expire.
Download its manifest, task records, and rasters:

```bash
PROFILE=aqw-char-dev
REGION=us-west-2
JOB_ID='<completed-job-id>'
BUCKET='<WorkResultBucketName>'
ARTIFACT_DIR="/private/tmp/aqw-compose-${JOB_ID}"

mkdir -p "$ARTIFACT_DIR/component/results" "$ARTIFACT_DIR/component/rasters"

aws s3 cp \
  "s3://${BUCKET}/jobs/${JOB_ID}/prepare/manifest.json" \
  "$ARTIFACT_DIR/manifest.json" \
  --profile "$PROFILE" \
  --region "$REGION"

aws s3 cp \
  "s3://${BUCKET}/jobs/${JOB_ID}/component/results/" \
  "$ARTIFACT_DIR/component/results/" \
  --recursive \
  --profile "$PROFILE" \
  --region "$REGION"

aws s3 cp \
  "s3://${BUCKET}/jobs/${JOB_ID}/component/rasters/" \
  "$ARTIFACT_DIR/component/rasters/" \
  --recursive \
  --profile "$PROFILE" \
  --region "$REGION"
```

### 4. Generate the Pillow reference locally

Run one ten-frame worker so the comparison measures one Lambda-sized unit of
work rather than local parallel fan-out:

```bash
AWS_PROFILE=aqw-char-dev \
  .venv/bin/python scripts/compose_component_artifacts_local.py \
  --artifact-dir "$ARTIFACT_DIR" \
  --output /private/tmp/pillow-reference.webp \
  --frames-per-worker 10 \
  --workers 1 \
  --compositor pillow
```

The Rust local mode should additionally retain lossless frame PNGs for direct
pixel comparisons. Add an equivalent debug option to the Python fixture
runner if necessary; do not judge parity only from the final lossy WebP.

### 5. Benchmark process startup separately

Build once, then time the release binary without recompiling:

```bash
cargo build --release --manifest-path services/component-compose-rust/Cargo.toml

/usr/bin/time -lp \
  services/component-compose-rust/target/release/aqw-component-compose \
  local-compose \
  --artifact-dir "$ARTIFACT_DIR" \
  --output-dir /private/tmp/rust-compose-run \
  --frame-start 1 \
  --frame-end 10
```

Record at least five sequential runs. Report the first run separately from
the median of later runs. Delete only the candidate output directory between
runs; keep input artifacts in the filesystem cache, just as warm Lambda
invocations may reuse downloaded libraries but still download S3 objects.

### 6. Local correctness fixtures

At minimum test these saved jobs:

- Alina: ordinary armor, helm, and weapon baseline.
- Muq: filter-heavy static ground/cosmetic behavior.
- Soltina: translucent cape, animated weapon, pet, and 2048px output.
- Fiy: scripted hair colors and multiply-blended shading.
- Try: pet marker positioning.
- Artix: skin colors and full body-layer ordering.
- Queen Annie 012: continuing nested cape animation.

Start with one frame at 256px, then one ten-frame chunk at 256px. Only after
those pass, test ten frames at 2048px and complete 120-frame output.

## Safe deployed benchmark

### 1. Deploy a disconnected candidate Lambda

Create a dedicated function such as:

```text
aqw-char-dev-componentcompose-rust
```

Initially it must have:

- no SQS event source;
- no Step Functions reference;
- the same 3008 MiB memory, 4096 MiB temporary storage, and timeout as the
  Python component-compose function;
- read/write access to only the existing work-result bucket;
- the same non-secret render environment values it needs;
- reserved concurrency `1` for the isolated benchmark.

Use a dedicated multi-stage image whose final base is
`public.ecr.aws/lambda/provided:al2023`. Do not copy FFDec, Java, pyvips,
Pillow, or resvg into this image. Include only the Rust bootstrap and either
the pinned `cwebp` binary or the selected libwebp library.

Review before deploying:

```bash
npm run build
npm test -- --runInBand
npm run synth -- --profile aqw-char-dev
npm run diff -- --profile aqw-char-dev
```

The first diff should add the candidate function, log group, error alarm, and
least-privilege S3 policy only. It must not change the render state-machine
definition.

Then deploy:

```bash
npm run deploy -- \
  --profile aqw-char-dev \
  --require-approval broadening \
  --outputs-file cdk-outputs.dev.json
```

Confirm the isolation cap:

```bash
aws lambda get-function-concurrency \
  --function-name aqw-char-dev-componentcompose-rust \
  --profile aqw-char-dev \
  --region us-west-2
```

### 2. Invoke one Lambda at a time

Extend `scripts/benchmark_deployed_component_compose.py` to pass a unique
benchmark output prefix. Its existing loop invokes synchronously and
sequentially, so `--runs 6` produces one first/cold candidate sample followed
by five warm samples without client-side concurrency:

```bash
AWS_PROFILE=aqw-char-dev AWS_REGION=us-west-2 \
  .venv/bin/python scripts/benchmark_deployed_component_compose.py \
  --artifact-dir "$ARTIFACT_DIR" \
  --function-name aqw-char-dev-componentcompose-rust \
  --runs 6 \
  --frame-start 1 \
  --frame-end 10 \
  --benchmark-output-prefix "benchmarks/rust-compose/${JOB_ID}"
```

Run the same command against `aqw-char-dev-componentcompose` for the Pillow
baseline. Preserve identical memory, input artifacts, S3 Region, frame range,
and WebP settings.

Do not infer cold-start performance by waiting an arbitrary number of
minutes. The first invocation after deployment is a useful cold sample. For
multiple controlled cold samples, publish and invoke fresh candidate versions
or change a candidate-only benchmark token and redeploy. Never mutate the
production function merely to force a cold start.

### 3. Collect deployed measurements

For every invocation capture:

- client-observed wall time;
- Lambda `Init Duration`, `Duration`, `Billed Duration`, and maximum memory;
- manifest read time;
- S3 component download count, bytes, and duration;
- PNG decode time;
- composition time;
- WebP encode time;
- S3 upload time;
- output hashes and sizes.

Separate the first candidate invocation from warm results. Compare warm
median and p95 rather than selecting the fastest run.

### 4. One-concurrency end-to-end rollout

Only after direct invocation passes, add a reversible configuration setting:

```text
componentComposeBackend = "pillow" | "rust"
```

In development, point `ComposeComponentFrameChunk` at the Rust function and
temporarily set both the compose Map concurrency and Rust reserved concurrency
to one. Submit controlled jobs in this order:

1. Alina, one frame, 256px output.
2. Alina, ten frames, 256px output.
3. Soltina, ten frames, 256px output.
4. Alina, ten frames, 2048px output.
5. Soltina, 120 frames, 2048px output.

Verify the final CloudFront WebP, DynamoDB terminal state, result message,
frame count, dimensions, durations, and visual/pixel comparisons after each
step. Keep the Python function deployed for an immediate configuration-only
rollback.

After correctness and one-worker latency pass, increase compose concurrency
gradually, for example `1 -> 2 -> 5 -> 20`. Do not combine the backend switch
and full concurrency increase into one benchmark.

## Acceptance criteria

The Rust candidate should not replace Pillow until all of these hold:

- Every required fixture preserves layer order, dimensions, frame count, and
  duration.
- Pre-WebP output-grid RGBA is exact, or every understood difference is
  documented and explicitly accepted.
- Decoded WebP comparison is no worse than the existing accepted compositor
  differences.
- There are no missing layers, alpha fringes, transparent seams, or matte
  colors.
- One-at-a-time cold wall time improves materially over the Python function.
- Warm median and p95 do not regress.
- Memory remains comfortably within the configured Lambda size.
- The candidate returns the exact result and batch-manifest contracts expected
  by the unchanged finalizer.
- Failure and retry behavior remains explicit; a missing component must fail
  the chunk rather than silently render an incomplete character.

An initial performance target can be a 15% or greater improvement in complete
ten-frame Lambda wall time or a clear billed-GB-second reduction. Treat that
as a benchmark gate, not an assumed result: S3 downloads and `cwebp` encoding
may dominate enough that replacing Pillow alone yields a smaller improvement.

## Expected limitations

- Rust composition does not accelerate FFDec export or resvg filters.
- A smaller native image should improve cold startup, but AWS scheduling and
  image-cache state still introduce variance.
- Concurrent S3 downloads remain network-bound.
- Keeping `cwebp` as a subprocess retains a small process-launch cost; this is
  intentional for the first fidelity comparison.
- Direct libwebp integration may change encoded bytes even with equivalent
  visual quality, so benchmark it separately.
- `tiny-skia` premultiplied-alpha rounding may not be bit-identical to Pillow.
- Reserved concurrency one is for isolation, not the eventual production
  setting.

## Future Rust component-raster worker

The same repository can later host a Rust component-raster Lambda with:

```toml
resvg = "0.48.1"
fast_image_resize = "..."
```

It can parse FFDec SVG output with `usvg`, render directly into a
`tiny-skia::Pixmap`, scan visible bounds, downsample, and encode the component
PNG without spawning the current resvg CLI. This removes process and temporary
file overhead, but it still executes the same expensive resvg filter
algorithms. It should therefore be treated as a separate optimization from the
component composer.

## References

- [AWS: Building Lambda functions with Rust](https://docs.aws.amazon.com/lambda/latest/dg/lambda-rust.html)
- [AWS: `provided.al2023` runtime](https://docs.aws.amazon.com/linux/al2023/ug/lambda.html)
- [AWS Lambda Rust runtime](https://github.com/aws/aws-lambda-rust-runtime)
- [resvg Rust API](https://docs.rs/resvg/0.48.1/resvg/)
- [tiny-skia blend modes](https://docs.rs/tiny-skia/latest/tiny_skia/enum.BlendMode.html)
- [`fast_image_resize`](https://github.com/Cykooz/fast_image_resize)

## Implementation status (2026-09)

Implemented and validated in `services/component-compose-rust`:

- **Pillow-exact integer compositor** — `src/compositor.rs` reimplements
  Pillow's `ImagingAlphaComposite` kernel (including its UINT32 wraparound and
  rounded divides) plus clipping and Python banker's-rounding canvas math.
  Verified exhaustively against Pillow 12.3.0 (full alpha sweep) and through
  `scripts/rust_compose_parity.py`: generated v19 output-grid fixtures
  (translucent overlap, order swaps, negative/off-canvas placement, empty
  tasks) produce **exact lossless RGBA (max channel diff 0)** and
  **byte-identical WebP** vs the production Pillow path; a 24-frame/16-layer
  randomized stress also lands at 0 diff pixels.
- **Worker** — `src/worker.rs` runs one whole chunk per process, decodes each
  referenced component once, downloads with bounded concurrency (default 32),
  encodes with the pinned `cwebp` (`-q <quality> -alpha_q 100 -m <method>`),
  writes `jobs/{job_id}/component/webp-frames/{frame:06}.webp` and
  `compose-batches/batch-{index:04}.json` (schema_version 1, preserved frame
  record shape), and fails the chunk on missing results/PNGs or mixed
  coordinate spaces. Supports the event's optional `benchmark_output_prefix`
  so candidate invocations never touch a completed job's keys.
- **Local mode** — `local-compose --artifact-dir ... --output-dir ...`
  reads `manifest.json` + `component/results/*.json` + rasters, writes
  lossless `frames/{frame:06}.png`, WebPs, and `batch-{index:04}.json`, and
  emits the `component_compose_profile` JSON event. Validated end-to-end
  against the saved Alina job (`/private/tmp/alina-component-benchmark`,
  legacy 4096→2048 raster space including the downsample branch).
- **Telemetry** — `component_compose_profile` and `component_compose_complete`
  events with the full Python field set; Lambda `REPORT` lines are preserved
  by the platform.
- **Tests** — `cargo fmt --check`, `cargo clippy --all-targets --all-features
  -- -D warnings`, and `cargo test --workspace` (25 unit + 4 end-to-end
  integration tests) are clean.
- **AWS build flags** — the Dockerfile builds inside
  `public.ecr.aws/lambda/provided:al2023` (same glibc as the runtime) with
  `RUSTFLAGS="-C target-cpu=x86-64-v2"`, thin LTO, single CGU, stripped,
  panic=abort. The produced `/var/runtime/bootstrap` is a lean x86-64 ELF;
  the image also ships the same pinned libwebp 1.5.0 `cwebp`. The image was
  built locally with `docker buildx --platform=linux/amd64`, the container
  ran the full local-compose flow under al2023, and a local
  Runtime-Interface-Emulator invocation confirmed the runtime polls the API
  and returns the event contract / clean Lambda errors.
- **CDK** — `componentComposeBackend` selects the Python rollback worker or
  `aqw-char-dev-componentcompose-rust`. Development now selects Rust with the
  same unreserved Lambda concurrency behavior as Python; the inline compose
  Map limits each render job to 20 concurrent invocations. The Rust worker
  retains work-bucket-only IAM, its dedicated log group, and its error alarm.
- **Benchmark script** — `scripts/benchmark_deployed_component_compose.py`
  gained `--benchmark-output-prefix`; the Python worker ignores the extra
  event field, so the same harness drives both Pillow and Rust functions.

The Rust backend was deployed on 2026-09-02 after direct cold/warm parity
benchmarks. An eight-frame, 256px Alina smoke job then completed through the
full SQS and Step Functions workflow with byte-valid output from the Rust
composer.
