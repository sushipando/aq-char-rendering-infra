# AQW Character Rendering Infrastructure

This repository owns the distributed `/char` rendering system: AWS CDK
infrastructure, Step Functions orchestration, Rust Lambda services,
container definitions, tests, and operational documentation. Rendering uses
FFDec only; it does not use AIR or Ruffle.

The dev stack is deployed. Local changes do not reach AWS until the repository
owner runs the deployment script. `cdk synth` is read-only, but `cdk bootstrap`
and `cdk deploy` create or change billable AWS resources.

## Development environment

| Setting | Value |
| --- | --- |
| AWS account | `538522204887` (`aqw-char-dev`) |
| AWS Region | `us-west-2` |
| AWS CLI profile | `aqw-char-dev` |
| CDK language | TypeScript |
| Renderer language | Rust (FFDec remains a pinned Java subprocess) |

Authenticate with temporary IAM Identity Center credentials:

```bash
aws sso login --profile aqw-char-dev
aws sts get-caller-identity --profile aqw-char-dev
```

Install and validate the TypeScript CDK application:

```bash
npm ci
npm run build
npm test
npm run synth -- --profile aqw-char-dev
```

Validate the Rust pipeline (see the [pipeline guide](services/pipeline-rust/README.md)
for local replay and rollout requirements):

```bash
cargo test --manifest-path services/pipeline-rust/Cargo.toml --locked
cargo clippy --manifest-path services/pipeline-rust/Cargo.toml --locked --all-targets -- -D warnings
```

The Python workspace remains available for legacy reference tests and offline
dataset tooling; it is not included in any deployed Lambda image:

```bash
uv sync --all-packages
uv run --package aqw-char-renderer pytest services/renderer/tests
uv run --package aqw-char-renderer ruff check services/renderer
```

Docker is required before deployment. ARM64 images contain native Rust
bootstraps and patched resvg. Only the exporter image adds FFDec 26.2.1 and
Java 21; composition/finalization use pinned libwebp tools.

## Tuning

All environment-specific tuning lives in
[`lib/config/environment.ts`](lib/config/environment.ts). It controls:

- Lambda memory, `/tmp`, timeout, and reserved concurrency per stage;
- raster and final output sizes, FFDec zoom, frame cap, frame batch size, Step
  Functions Map concurrency, and Q85 WebP settings;
- official AQW missing-asset fallback and timeout;
- per-Discord-user active job limit;
- S3/DynamoDB/log retention;
- queue/workflow timeouts and monthly budget shutdown threshold.

The launcher hydrates sparse requests from this configuration. The Discord
`/render` command exposes the render, animation, fan-out, item-override, WebP,
and per-cache controls. When `raster_size` is omitted, the bot derives it as
twice `output_size` (capped at 4096). Matching sizes still skip resampling,
while larger rasters are downsampled once before WebP encoding.

## Architecture

```text
Discord bot -> DynamoDB admission transaction -> job SQS -> launcher
  -> Step Functions Standard
      -> PrepareResolve (final-cache fast path)
      -> ExportSourceFrames (one FFDec invocation per source; unique SVGs -> bounds SQS prefetch)
      -> PlanBounds (global SVG dedup, completed-result check, and correctness barrier)
      -> ProbeUniqueStatesInline (request-selected direct Rust resvg)
         or ProbeUniqueStatesDistributed (request-selected S3 -> Standard children -> SQS callback)
      -> PrepareFinish (validated bounds, schedules, shared canvas)
      -> RasterComponentStatesInline (request-selected direct patched resvg)
         or RasterComponentStatesDistributed (request-selected Express children)
      -> ComposeComponentFrameChunks (Rust composition and WebP encoding)
      -> FinalizeAnimation (validated WebP mux, publish, inline completion)
      -> result SQS -> Discord bot

CloudFront -> private S3 /renders/ objects
```

Each source exporter uses an inotify-backed filesystem watcher to discover
completed SVGs while its FFDec subprocess is still generating later frames,
then durably stores and submits each unique SVG as a fire-and-forget bounds
task. This lets small resvg probes overlap the same source export as well as
other source exports. `PlanBounds` still runs after
every exporter completes, reuses valid prefetched results (including job-scoped
no-cache results), and sends only unfinished work through the request-selected
Inline or Distributed path.

Before SVG generation, Rust resolves supported nested MovieClip state controls
and normalizes an export-only SWF. Stopped controllers keep their idle artwork
animated without advancing into Walk/Attack. Unsupported reachable controls fail
with a sprite/frame diagnostic instead of silently guessing. See
[nested timeline resolution](docs/nested-idle-timeline-resolution.md) for behavior,
limits, cache invalidation, and local regression commands.

`FinalizeAnimation` losslessly combines adjacent identical encoded frames by
extending their duration. It preserves logical `frame_count` and reports
`physical_frame_count` and `merged_frame_count` separately. A constant animation
retains a timed animation frame, and oversized duration runs are split safely.
This is automatic; no generation flag is required. Only the final-result cache
identity changes. See the [audit implementation addendum](docs/render-speed-quality-audit-2026-09-06.md#implementation-addendum-adjacent-run-merging)
for validation results and repeatable local tests.

The bulk SWF corpus is checksummed and immutable. If a live character refers
to a legitimate staff/legacy asset absent from that corpus, Prepare can fetch
only from `https://game.aq.com/game/gamefiles/`, validate the SWF header, and
atomically preserve the first copy in the private versioned source bucket.

## Deployment sequence

Deployments are operator-run only: agents and unattended automation may prepare,
test, synthesize, and review a diff, but must not invoke the deployment command.
This is a test bot, so normal deployments do not pause admissions or drain the
queue; failed in-flight jobs are acceptable during a rollout.

From the repository root, the owner runs:

```bash
scripts/deploy_renderer.sh --yes
```

The script validates the AWS account/profile, checks Docker, runs the build,
tests, synth, and diff, then deploys the dev stack. Add `--smoke USERNAME` to
submit that character as a resvg render after deployment; bare `--smoke`
defaults to `alina`. `--skip-checks` is available only when the same revision
has already passed those checks.

The equivalent manual review sequence is:

Review the synthesized template and tuning first:

```bash
aws sso login --profile aqw-char-dev
npm run build
npm test
npm run synth -- --profile aqw-char-dev
npm run diff -- --profile aqw-char-dev
```

Bootstrap once per account/Region, then deploy only after reviewing the diff:

```bash
npx cdk bootstrap aws://538522204887/us-west-2 --profile aqw-char-dev
npm run deploy -- --profile aqw-char-dev --require-approval broadening
```

Copy the `SourceAssetBucketName` stack output and upload the source corpus. The
dataset version must equal `assetDatasetVersion` in the tuning file:

```bash
uv run --package aqw-char-renderer python scripts/bootstrap_source_assets.py \
  --bucket '<SourceAssetBucketName>' \
  --dataset-version dev-v1 \
  --asset-root ../aq-image-search/bot/assets/swf_item_index/swf_assets \
  --database ../aq-image-search/bot/assets/swf_item_index/item_db.json \
  --character-renderer ../swf/characterB.swf \
  --workers 12
```

Finally, attach the `HetznerBotManagedPolicyArn` output to credentials dedicated
to the Discord bot and configure these values in `aq-image-search/.env` from
the stack outputs:

```dotenv
CHAR_RENDER_ENABLED=true
AWS_REGION=us-west-2
CHAR_RENDER_JOB_QUEUE_URL=<JobQueueUrl>
CHAR_RENDER_RESULT_QUEUE_URL=<ResultQueueUrl>
CHAR_RENDER_JOB_TABLE=<JobTableName>
CHAR_RENDER_ENABLED_PARAMETER=<RenderEnabledParameterName>
CHAR_RENDER_MAX_ACTIVE_PARAMETER=<MaximumActivePerUserParameterName>
CHAR_RENDER_SOURCE_BUCKET=<SourceAssetBucketName>
CHAR_RENDER_ASSET_DATASET_VERSION=<AssetDatasetVersion>
CHAR_RENDER_SOURCE_TIMEOUT_SECONDS=15
```

Do not place an AWS secret in Git. On Hetzner, load the dedicated credentials
through a root-readable systemd environment file or an equivalent secret
mechanism.

The stack outputs `CloudFrontBaseUrl`, but the bot does not need it: trusted
result messages carry the complete `/renders/...` URL.

AQW blocks AWS Lambda egress from its character and gamefiles endpoints. The
bot therefore fetches the small public FlashVars response before admission and
immutably seeds only corpus-missing SWFs under the source bucket's
`dynamic-assets/<dataset>/` prefix. Its managed policy can read only the frozen
manifest/dynamic objects and write only that versioned dynamic prefix; all
FFDec, SVG, PNG, and WebP work remains in Lambda.

## Safety behavior

- A DynamoDB transaction reserves a user slot before SQS admission.
- Queued and running jobs count against the configurable per-user limit.
- Standard workflow retries and deterministic job IDs make launches
  idempotent.
- Terminal failures and scheduled reconciliation release leaked slots.
- The bot deletes a result message only after Discord delivery is recorded.
- S3 work objects expire after two days and renders after thirty days by
  default.
- The AWS Budget notification turns off the SSM admission switch, disables the
  launcher mapping, sets renderer concurrency to zero, and stops running
  workflows while preserving stored data.
- CloudFront rejects paths outside `/renders/`; both S3 buckets remain private.

Do not run `cdk bootstrap` or `cdk deploy` casually. Both mutate the AWS
account, and deployed resources can incur charges. The intended bootstrap
environment is:

```text
aws://538522204887/us-west-2
```

## Repository layout

```text
bin/                  CDK application entry point
lib/                  stacks, constructs, and environment configuration
services/renderer/    Python rendering and Lambda/container code
services/component-compose-rust/  Rust component-compose Lambda (in production)
services/component-raster-rust/   Rust component-raster Lambda candidate (resvg in-process)
test/                 CDK unit tests
docs/                 architecture and operating documentation
```

### Submitting renders

`scripts/render-character` queues real character renders through the deployed
workflow (same SQS + DynamoDB admission as the Discord bot) and waits for the
delivered CloudFront WebP. It supplies the dev AWS profile, Region, and `uv`
command automatically:

```bash
scripts/render-character alina -s 1024 -n 30 -q 70 -m 2
scripts/render-character artix --facing left --zoom 1 --padding 8
scripts/render-character alina --item-id 12345 --slot weapon --lossless
scripts/render-character mck -s 1024 --bounds-mode inline --component-raster-mode inline --no-cache
scripts/render-character mck -s 1024 --bounds-mode inline --component-raster-mode distributed --no-cache
```

Multiple characters queue one job each; `--webp-lossless` selects lossless
encoding; `--max-frames` controls the animation length; `--no-watch` queues
without waiting; `--no-verify` skips the CloudFront fetch (`--help` for all
options). Use `scripts/render-character --help` for facing, hidden/base items,
loop, start-frame, zoom, raster/output size, padding, item override, and WebP
controls. The deployed request contract is resvg-only.

Restart an existing job with the same appearance/assets, colors, render settings,
and Map modes, but a fresh job ID and the currently deployed workflow:

```bash
scripts/render-character --restart eccabdb9-d329-4e3d-a0a8-5b30eb7425df --dry-run
scripts/render-character --restart eccabdb9-d329-4e3d-a0a8-5b30eb7425df
# Skip the completed-result shortcut while retaining intermediate caches:
scripts/render-character --restart eccabdb9-d329-4e3d-a0a8-5b30eb7425df --no-render-cache
# Force every cache off for a cold-path comparison:
scripts/render-character --restart eccabdb9-d329-4e3d-a0a8-5b30eb7425df --no-cache
```

Restart goes through the same DynamoDB admission → SQS → launcher →
`PrepareResolve` path as any normal job. It is not redrive or compose-only resume:
no old rasters are copied, and the old job is not modified. The original cache
settings apply by default, including `CachedResultExists`; cache hits can still
skip work. Individual cache-disable flags are also available. The normal result
notification targets the original Discord user/channel.

The command reads the hydrated execution request, preserving the original asset
selection rather than fetching today's equipment. When the request did not
contain an appearance snapshot, it uses saved preparation fields; if neither
snapshot remains, it refuses to silently change assets. Otherwise old temporary
rasters/SVGs are not required. `--dry-run` prints the proposed request without
writes. An execution ARN or console-created UUID execution name is also accepted.
Use `scripts/render-character --restart --help` for restart-specific options.
This CLI works with the normal deployed workflow; no special restart Lambda or
state-machine branch is needed. Workflow changes themselves still require an
operator deployment.

Raster Map iterations now return scalar acknowledgements, and their aggregate
is discarded. `CollectComponentResults` validates all expected records and
writes `jobs/JOB/component/manifest.json`; compose receives only its S3 key,
not an accumulated raster-results array. This works for both raster Map modes.

`--bounds-mode inline|distributed` and
`--component-raster-mode inline|distributed` are independent per-job fan-out
switches, and both default to `inline`. Each is stored on the submitted job;
the state machine never selects a mode from task count. Inline Maps run up to
40 iterations concurrently and process additional work in later waves. Bounds
planning fails with instructions to use distributed mode if its inline task
data would make the Step Functions state unsafe. Use `--no-cache` when
comparing modes so a render, bounds, or component cache hit cannot bypass work.

`--no-cache` forces a benchmark render to bypass every cross-job compute cache.
The individual controls are `--no-render-cache`, `--no-animation-cache`,
`--no-vector-cache`, `--no-bounds-cache`, and `--no-component-cache`.
They bypass the completed render, animation-loop metadata, FFDec/vector export,
SVG bounds, and appearance-independent component-raster caches respectively.
Uncached bounds and vector manifests are written under the job prefix, so
later stages cannot consume older shared results. Exact source SWFs and the
newly generated SVG blobs remain immutable pipeline inputs, not cache hits.

### Benchmarking WebP against AVIF

`scripts/benchmark_webp_avif.py` decodes an animated WebP to lossless RGBA PNG
frames, benchmarks equivalent lossy and lossless WebP/AVIF encodes, verifies
alpha and lossless round trips, and writes machine-readable results. Its quick
default samples 12 evenly spaced frames:

```bash
scripts/benchmark-webp-avif
```

This defaults to `~/Desktop/annie-large.webp`; pass another input path when
needed. Add `--full-animation` to also compare `img2webp` with AVIF sequence
encoding over every frame, preserving source frame durations. The script
requires the `cwebp`, `img2webp`, `avifenc`, and `avifdec` command-line tools.

### Rust component workers

The component pipeline runs two native Rust Lambdas (see
`docs/rust-component-compose-plan.md`):

- `component-compose-rust` — composed components are decoded once, blended
  with a Pillow-exact integer compositor, and encoded with the pinned cwebp;
  selected through `componentComposeBackend`.
- `component-raster-rust` — rasterizes each component with resvg/usvg linked
  as a library and downsamples with a Pillow-exact Lanczos resampler
  (fast_image_resize remains available via `AQW_DOWNSAMPLER`). Production
  builds compile resvg only; ThorVG remains an opt-in local comparison feature.

Both use the same `provided:al2023` Dockerfile build with
`RUSTFLAGS="-C target-feature=+neon"` for ARM64 and pass the local parity harnesses
(`scripts/rust_compose_parity.py`, `scripts/rust_raster_parity.py`) with
pixel-exact RGBA against the Python workers.

No AWS credentials, SSO cache files, downloaded SWFs, render frames, or final
animations belong in Git.
