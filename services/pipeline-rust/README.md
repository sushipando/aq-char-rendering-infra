# Rust SVG-bounds pipeline

Implementation of `aq-image-search/docs/rust-svg-bounds-pipeline-plan.md`.
Implemented locally; not deployed or production-benchmarked.

All Lambda handlers are Rust: launcher, resolve, export, bounds planning and
probing, finish, finalization, completion, reconciliation, and shutdown. The
existing Rust raster and composition workers remain separate crates. FFDec
26.2.1 remains a Java subprocess, libwebp remains native, and CDK remains
TypeScript. The Discord bot and offline dataset tools are outside this Lambda
migration and remain Python. Legacy Python renderer code is retained for
reference, not deployed as a fallback handler.

## Data flow and cache contracts

`PrepareResolve -> ExportSourceFrames -> PlanBounds ->
ProbeUniqueStatesInline|ProbeUniqueStatesDistributed -> PrepareFinish ->
RasterComponentStatesInline|RasterComponentStatesDistributed ->
ComposeComponentFrameChunks -> FinalizeAnimation`.

Export runs per source SWF, grouping its symbol requests. FFDec has a deadline
shorter than the invocation deadline and is killed on timeout. Export hashes
the effective SVG after settled-child registration corrections, preserves
ordered schedules and the eight-frame validation tail, and never probes a
raster itself. While FFDec is still running, an event watcher backed by inotify
on Lambda Linux discovers each completed, parseable SVG, stores it durably, and
publishes a fire-and-forget task to the bounds SQS queue. This overlaps probes
with later SVG generation in the same FFDec invocation as well as work in other
source Lambdas. A final
manifest pass reconciles missed files and publishes any effective SVGs produced
by settled-frame registration corrections. It publishes the source manifest
only after its referenced SVG uploads and queue attempts.
Queue publication failure does not fail vector export because the later bounds
barrier detects and dispatches unfinished work. A vector-manifest cache hit
does not enqueue redundant prefetch messages; `PlanBounds` handles any
independently disabled or missing bounds results.

All new intermediate/cache objects are in the work bucket:

| Prefix | Contents / identity |
| --- | --- |
| `vector-svg/6/<sha256>.svg` | Exact effective SVG bytes, shared across frames/sources/jobs |
| `vector-manifests/6/<identity>.json` | Source hash, requests, FFDec/export-policy version, zoom/start/count, ordered schedules, CC/placement and animation metadata |
| `source-metadata/<policy>/<ffdec>/<sha256>.json` | Cached script metadata used by vector export |
| `svg-bounds/v1/<identity>.json` | SVG hash, patched-renderer policy, resolution/retry/padding/zoom, visibility and registration-space bounds |
| `component-rasters/2/<identity>.*` | Reusable raster/result for appearance-independent placed components |
| `jobs/<job>/prepare/` | Small source references, S3 probe task dataset, plan, verified bounds join and component manifest |
| `renders/v20-rust-bounds/` | Validated final WebP and cache-completion metadata sidecar |

Vector and bounds caches are independent of the final-render cache switch.
Existing librsvg bounds and old vector archives cannot become new-policy hits.
Shared cache prefixes do not currently expire automatically; job artifacts and
final results retain the existing lifecycle rules. Bump `EXPORT_POLICY`,
`BOUNDS_POLICY`, and/or `rendererVersion` when their associated semantics change.

The planner globally deduplicates exact SVG hashes, not thumbnail pixels, and
selects only results that are still missing after prefetch. Each request
explicitly selects `bounds_mode` as `inline` or `distributed`; missing fields
are hydrated to `inline` for older clients. The planner never changes that
selection based on task count. Inline Map runs at most 40 probes concurrently
and processes any remaining probes in later waves, subject to a conservative
state-payload guard. Distributed mode uses the S3 JSON ItemReader and Standard
SQS callback children, keeping the task list out of workflow payloads. Both
paths form a completion barrier; an empty task list bypasses both Maps. Finish
verifies every required result again before calculating the shared canvas.
Raster tasks reference one `svg_key` and verified hash, not a duplicate-frame
archive; the raster worker still accepts legacy `bundle_key` tasks.

Requests may independently disable completed-render, animation-loop metadata,
vector-export, bounds, and appearance-independent component-raster cache reuse.
Bounds-cache bypass uses job-scoped result keys, so finish cannot read a
pre-existing shared result. Export prefetch and the barrier can reuse that same
job-scoped result without enabling cross-job caching. Vector bypass reruns
FFDec and script metadata extraction and publishes a job-scoped source
manifest. Component bypass skips both shared reads and writes. Exact source
SWFs and generated SVG blobs remain content-addressed pipeline inputs rather
than mutable cache entries.

Component raster fan-out is independently selected per request through
`component_raster_mode`. Missing fields default to `inline`. Inline mode invokes
the Rust raster Lambda directly with concurrency 40; distributed mode uses
Express child executions with concurrency 200. Both process exactly one unique
placed component state per iteration, return the same result schema, and join
at `ComposeComponentFrameChunks`. Neither mode is selected automatically from
the task count.

## Bounds policy and retries

The worker uses the same vendored resvg 0.48.1 with `simd-blur` as final raster,
in-process, without PNG encoding or temporary images. Default maximum dimension
is 256 pixels, with one whole probe pixel of padding on each edge. Integer
thumbnail dimensions, whole occupied cells (`last + 1`), SVG viewport and
FFDec registration transform are all included in the coordinate conversion.

Empty, all-faint, or isolated faint-outlier probes retry at 1024. The union of
both probes is retained. If both are empty, the result is explicitly uncertain
and keeps conservative header bounds; it is not silently declared invisible.
Only provably empty documents/zero pages are confirmed invisible. Invalid SVGs,
hashes, unsupported registration transforms, or corrupt cache entries fail.
One-pixel padding is an estimate, not proof against arbitrarily faint features
absent in an otherwise nonempty thumbnail. Conservative invisible fallbacks
can also increase the canvas; representative visual validation remains required.

Worker writes are immutable and precede callbacks. The same SQS worker accepts
both fire-and-forget exporter prefetch messages and task-token callback messages.
Duplicate deliveries, barrier races, and callback failures reuse stored results.
Expired/already-completed callback
tokens are acknowledged; transient callback errors retry. Probe failure on
the third delivery sends task failure. Crashes/poison messages that cannot
callback reach the DLQ; the explicit workflow deadline still fails the job.
Terminal completion atomically releases user capacity and retries result
publication through the existing cleanup path. Result messages remain at-least-once.
Tokens are not logged, and workflow logging excludes execution data.

Initial tuning (centralized in `lib/config/environment.ts`):

- Bounds worker: 1024 MiB, 60-second timeout, SQS batch size one with partial failures.
- Bounds Inline Map: up to 40 direct invocations at once, selected per request.
  Distributed mode uses up to 100 SQS-backed workers; queue visibility is 360
  seconds with three receives and a 1200-second callback timeout.
- Bounds resolution/padding: 256/1. Defaults are configurable, not benchmark claims.
- Component raster: request-selected Inline concurrency 40 or Distributed
  Express concurrency 200; Inline is the request default.
- Export: 3008 MiB, 300-second timeout, eight concurrent source exports.
- Final component raster/composition concurrency and frame caps are unchanged.
- Shutdown disables both queue mappings and includes both new functions.

## Local verification

```bash
cargo test --manifest-path services/pipeline-rust/Cargo.toml --locked
cargo clippy --manifest-path services/pipeline-rust/Cargo.toml --locked --all-targets -- -D warnings
cargo test --manifest-path services/pipeline-rust/Cargo.toml --locked -p aqw-component-raster --lib
npm run build
npm test -- --silent
```

Probe an FFDec SVG without AWS configuration:

```bash
cargo run --release --manifest-path services/pipeline-rust/Cargo.toml -- probe-svg /path/frame.svg 1
```

Synthetic end-to-end test (single-frame and animated WebP, including duplicate
and missing batch rejection):

```bash
AQW_TEST_CWEBP=/path/to/cwebp CHAR_RENDER_WEBPMUX=/path/to/webpmux \
  cargo test --release --manifest-path services/pipeline-rust/Cargo.toml \
  --test pipeline full_rust_pipeline_encodes_and_validates_webp -- --ignored
```

Incident ground cold/warm replay, without uploading anything:

```bash
AQW_TEST_SWF=/path/to/LaeDWearNeonDragonr1.swf \
AQW_TEST_FFDEC=/path/to/ffdec-cli.jar \
  cargo test --release --manifest-path services/pipeline-rust/Cargo.toml \
  --test pipeline incident_ground_cold_export_and_warm_cache -- --ignored --nocapture
```

`replay_incident_through_all_rust_stages` additionally takes `AQW_TEST_STORE`:
an existing local fixture directory containing the incident's `input.json` and
downloaded SWFs under `source/<original S3 key>`, plus the three tool paths
above. It writes only local `work/` and `preview-png/` artifacts. Private assets
and credentials are not checked in. The fixture deliberately exports all 128
source frames but limits final output to 12 frames, 512px raster / 256px output.

Observed locally on 2026-09-05:

- Ground incident: 128 frames, 104 unique SVGs, mirror boundary 49; exporter
  performed no raster work. A separate cold release-mode export took 4.18s;
  warm export took 4.2ms and succeeded with an unavailable FFDec jar. These
  timings use local filesystem storage, not S3 or Lambda.
- All six incident sources: 2,048 symbol-frames -> 149 unique SVGs. Full local
  Rust replay completed in 152.47 seconds with sequential probes/raster tasks,
  producing a 12-frame, 500ms, 256x166 WebP of 219,998 bytes. The warm bounds
  planner found 149 hits and zero missing tasks. Preview inspected visually.
- Synthetic 4096px comparisons cover thin strokes, glows, faint pixels,
  mirrored parts, zoom 1/2 and padding 1/2. Callback/dedup/barrier, malformed
  inputs, SWF timelines/CC, settled registration and final container tests pass.
- ARM64 exporter image built and its Java 21/libwebp 1.5.0 packaging smoke-tested.
  Native end-to-end WebP tests used locally installed libwebp 1.6.0.

These are local correctness checks, not an AWS latency comparison or complete
visual parity certification. No renderer-performance conclusions are drawn
about the earlier cold-versus-cached RasterComponentStates runs.

## Containers

Use `services/` as the build context so all three Rust crates and vendored
resvg are available. From the repository root:

```bash
docker build --platform linux/arm64 --target runtime -f services/pipeline-rust/Dockerfile -t aqw-pipeline:local services/
docker build --platform linux/arm64 --target exporter -f services/pipeline-rust/Dockerfile -t aqw-exporter:local services/
```

CDK selects the correct context and target automatically. `CHAR_RENDER_HANDLER`
selects the native handler; only the `exporter` target carries Java/FFDec.

## Initial migration rollout (completed)

1. Review/synthesize the CDK diff; this implementation has not been deployed.
2. Disable new bot admissions and allow queued/in-flight jobs to drain. Do not
   use the budget kill switch for a graceful drain: it also stops executions.
   Old preparation manifests cannot finish against the new handlers.
3. Deploy the bot's resvg-only request default/choice and the renderer stack
   together. Explicit legacy `thorvg` requests are rejected by the new contract.
4. Run the failed job at its full 120-frame, 4096px raster / 2048px output
   settings, then a duplicate. Verify export has no probes, cache hits enqueue
   no bounds work, workflow timing, frame timing/colors and absence of clipping.
5. Test real AWS callback interruption, worker timeout and exhausted retries;
   confirm terminal result delivery and user-slot release. Native tests use
   fake callbacks/local storage, not a live DynamoDB/SQS chaos test.
6. Compare more representative ground effects, invisible/blink states and
   glows at full resolution before accepting the probe policy and tuning
   production concurrency. Re-enable admissions after those checks.

Rollback requires another drain and redeploying the previous CDK/images; old
Python code is retained for that reference, not silently switched at runtime.

For subsequent test-bot deployments, do not pause admissions or drain the
queue. The repository owner runs `scripts/deploy_renderer.sh --yes`; agents and
unattended automation stop after validation and diff review.
