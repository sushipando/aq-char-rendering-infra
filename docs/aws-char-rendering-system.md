# Distributed `/char` Rendering System

Status: implemented and validated locally; CDK synthesizes successfully, but
the AWS stack has not been bootstrapped or deployed.

Last updated: 2026-08-28.

This document records the design and operating requirements for rendering an
animated AQW character from a username, storing the result in S3, and
displaying it in Discord without doing rendering work on the small Hetzner
search server.

## Decisions already made

- The production output is an animated WebP with transparent pixels.
- Use lossy WebP RGB quality 85 and lossless alpha.
- The production maximum image dimension is initially 2048 pixels.
- Preserve the SWF frame rate by default. `characterB.swf` currently reports
  24 FPS, represented by a drift-free sequence of 41 ms and 42 ms WebP frame
  durations.
- Detect a complete combined animation loop, ignore standard blink-only
  timelines as loop drivers, and cap output at a configurable maximum. Start
  with a 360-frame cap because Soltina's detected loop is 360 frames.
- Use FFDec only. Do not use Ruffle. Do not use AIR for the distributed SVG
  compositor.
- Render work runs in Lambda containers, not on the Hetzner bot server.
- Put SQS in front of the workflow for durable admission.
- Use AWS Step Functions Standard Workflows to coordinate parallel frame
  batches and the barriers between stages.
- Keep S3 private and serve final results through CloudFront Origin Access
  Control. API Gateway is not needed.
- Limit simultaneous jobs per Discord user with an atomic DynamoDB operation.
  Queued jobs count toward the limit.
- Do not pay for provisioned concurrency. Cold starts of roughly five seconds
  are acceptable.
- Reserved concurrency and Step Functions `MaxConcurrency` are safety limits;
  unused concurrency has no compute charge.

Configuration values must remain configurable rather than being scattered as
constants. The values above are initial production defaults.

## Current source state

The reference renderer still lives in the sibling `aq-image-search` repository
at `pipeline/render_swf_character_svg.py`. A fidelity-preserving copy and its
tests now live under `services/renderer/` here and provide the following:

- Fetches or loads saved public character FlashVars.
- Resolves cosmetic/base armor, weapon, helm/hair, cape, and ground SWFs.
- Supports one SWF, item-ID, or SVG slot override per invocation.
- Uses the decompiled `characterB.swf` display order and transforms.
- Exports nested item states through headless FFDec.
- Detects a complete combined loop and ignores standard blink timelines.
- Composes faithful SVG frames, including color customization.
- Calculates a tight visible viewBox for every frame.
- Unions all frame viewBoxes into one shared character-space canvas to prevent
  animation jitter.
- Recalibrates FFDec minimum strokes after the final scale is known.
- Rasterizes SVG frames with `rsvg-convert`.
- Computes even-offset delta rectangles between adjacent RGBA frames.
- Encodes frames concurrently with `cwebp` and combines them with `webpmux`.

The AWS package separates versioned contracts, storage, orchestration stages,
and Lambda handlers while retaining the proven composition operations.

Live character pages sometimes reference legitimate staff or legacy SWFs that
are absent from the bulk item corpus. The implemented resolver permits only
the fixed official AQW `gamefiles` origin, validates the SWF, and atomically
caches the first copy in the private versioned source bucket. Its content hash
is included in the render hash and job manifest.

Source commit `e7aa745` added:

```text
--webp-lossy-quality 0-100
```

Passing `--webp-lossy-quality 85` uses lossy RGB plus lossless alpha. Omitting
the option preserves exact-lossless RGBA behavior. Preserve this behavior.
The full local unit suite passed 184 tests after this addition.

Relevant source functions include:

- `export_requested_symbol_frames()` for grouped FFDec export.
- `compose_svg()` for character composition and per-frame visible bounds.
- `align_frame_svg_viewboxes()` for shared-canvas union and stroke
  recalibration.
- `render_preview()` for SVG-to-PNG rasterization.
- `animation_delta_crop()` for WebP delta rectangles.
- `save_animation_webp_parallel()` for concurrent `cwebp` plus final
  `webpmux`.

The large SWF corpus and item database are local runtime assets in the sibling
source repository, not normal Git artifacts:

```text
bot/assets/swf_item_index/item_db.json
bot/assets/swf_item_index/swf_assets/
```

The source working tree also contains large untracked render outputs and
downloaded assets. An implementation agent must inspect `git status` in both
repositories and stage explicit source paths; do not use a broad `git add -A`
that accidentally commits the asset corpus, generated frames, credentials, or
logs.

The renderer currently expects the official character renderer locally. Pin a
verified copy and checksum for the Lambda build:

```text
https://game.aq.com/game/gamefiles/etc/chardetail/characterB.swf
```

FFDec 26.2.1 is the locally verified version at the time of this design. Pin
the exact FFDec JAR/container dependency instead of silently using the latest
release.

## Goals

1. `/char` accepts an AQW username and produces a faithful transparent animated
   WebP.
2. Hetzner performs only Discord, DynamoDB, and SQS operations.
3. One render uses several parallel Lambda invocations for lower latency.
4. A render remains deterministic, retryable, and safe under at-least-once
   delivery.
5. Identical resolved appearances and settings reuse a cached final result.
6. Users cannot exceed a configurable number of queued/running generations.
7. Failed, timed-out, or aborted workflows always release user capacity.
8. Intermediate and final artifacts expire automatically.
9. The whole deployment is defined by infrastructure as code and can be
   reproduced in a staging AWS account/stack.

## Non-goals for the first release

- No public HTTP rendering API.
- No API Gateway.
- No Ruffle or AIR renderer.
- No arbitrary user-supplied URLs or arbitrary SWFs.
- No provisioned concurrency.
- No multi-region active/active deployment.
- No requirement to keep Discord results forever.
- Do not implement one Lambda invocation per frame.

## High-level architecture

```text
Discord /char
    |
    | defer immediately, reserve user slot, enqueue small request
    v
SQS char-render-jobs  ---> DLQ
    |
    v
Launcher Lambda
    |
    | StartExecution(name=job_id)
    v
Step Functions Standard Workflow
    |
    +--> Prepare Lambda (reserved concurrency 1)
    |       resolve appearance, FFDec export, loop detection, cache check,
    |       shared-canvas viewBox from export headers, per-part frame archives
    |
    +--> Map: Render frame batches
    |       compose in memory -> rasterize once on the shared canvas
    |       -> delta Q85 frame WebP
    |
    +--> Finalizer Lambda
    |       ordered webpmux -> final S3 object -> inline job completion
    |
    +--> SQS char-render-results
            |
            v
Hetzner result listener
    |
    +--> edit deferred response when token is valid
    +--> otherwise send a normal channel message
            |
            v
Discord embed image URL
            |
            v
CloudFront ---> private S3 result object
```

SQS admission and Prepare concurrency of one do **not** serialize an entire
generation. As soon as Job A finishes Prepare, Job B may enter Prepare while
Job A's Map states continue. Global worker reserved concurrency limits total
rendering pressure across all active workflows.

## AWS resources

Implement all resources with the TypeScript AWS CDK application in this
repository. Keep CDK dependencies in the root Node project and Python rendering
dependencies in the `uv` workspace under `services/renderer/`.

Use one configurable AWS region for Lambda, Step Functions, SQS, DynamoDB, and
S3. The dedicated development environment is account `538522204887` in
`us-west-2`. Preserve environment configuration as a distinct module so future
production accounts and Regions do not require scattered code changes.

Required resources:

1. ECR repositories for the renderer Lambda image or images.
2. Private source-asset S3 bucket.
3. Private work/result S3 bucket.
4. `char-render-jobs` SQS queue and dead-letter queue.
5. `char-render-results` SQS queue and dead-letter queue.
6. Launcher Lambda subscribed to the job queue with batch size one.
7. Prepare Lambda.
8. Render worker Lambda (compose + rasterize + encode per batch).
9. Finalizer Lambda (mux + publish + inline completion).
10. Complete Lambda (cache-hit and failure terminal paths).
11. Cleanup/reconciliation Lambda.
12. Step Functions Standard state machine.
13. DynamoDB job/concurrency table.
14. EventBridge rule for terminal Step Functions statuses.
15. CloudFront distribution with Origin Access Control for the result prefix.
16. CloudWatch log groups, metrics, alarms, and a cost budget.

Initially, one shared renderer container image is simpler and acceptable
because cold-start latency is not sensitive. It should contain:

- A pinned Linux Python runtime.
- A pinned headless Java runtime.
- The pinned FFDec CLI JAR.
- The pinned `characterB.swf`.
- The reusable renderer Python modules.
- `rsvg-convert`/librsvg.
- Pillow.
- `cwebp` and `webpmux` from a pinned libwebp release.
- The AWS Lambda runtime interface client.

Do not bake the complete item SWF archive into the image. Put source SWFs in
the source-asset bucket and download only the assets required for a character.
Warm `/tmp` contents may be reused opportunistically, but correctness must not
depend on Lambda environment reuse.

Do not attach these Lambdas to a VPC initially. They need public access to AQW
and AWS APIs, and a VPC would introduce NAT cost and configuration without a
current requirement.

## Initial resource sizing

These are starting values to benchmark, not permanent truths:

| Function | Memory | `/tmp` | Timeout | Reserved concurrency |
| --- | ---: | ---: | ---: | ---: |
| Launcher | 512 MB | 512 MB | 30 s | 1 |
| Prepare | 4-7 GB | 4 GB | 900 s | 1 |
| Render worker | 3-4 GB | 4 GB | 900 s | 8 globally |
| Finalizer | 2-4 GB | 4 GB | 300 s | 1-2 |
| Complete | 512 MB | 512 MB | 60 s | 1-2 |
| Cleanup | 512 MB | 512 MB | 60 s | 2 |

Start each Map at `MaxConcurrency: 4`, then test `8`. Use 15-30 frames per
batch. Twelve batches of 30 frames for a 360-frame animation are far better
than 360 one-frame cold starts.

Lambda CPU is proportional to configured memory and tops out at six vCPUs at
10,240 MB. Once work is distributed between Lambda invocations, start with one
or two local threads per frame worker rather than blindly retaining four local
threads in every Lambda.

Setting reserved or maximum concurrency has no direct charge. Compute billing
is approximately the sum of allocated GB multiplied by execution seconds over
all invocations. With perfect five-way scaling, five equal-memory Lambdas for
200 seconds cost approximately the same compute as one for 1,000 seconds, plus
duplicated initialization and orchestration overhead. Note that one Lambda
cannot actually run for 1,000 seconds; its maximum is 900 seconds.

## S3 layout and lifecycle

Use separate source and output buckets to reduce the risk that an expiration
rule deletes source assets.

Suggested source bucket keys:

```text
character-renderer/<characterB-sha256>/characterB.swf
swf/<normalized-game-path>.swf
databases/<item-db-version>/item_db.json
```

Suggested work/result bucket keys:

```text
jobs/<job-id>/request.json
jobs/<job-id>/prepare/manifest.json
jobs/<job-id>/prepare/parts/<symbol>.tar.gz
jobs/<job-id>/render/batch-000.json
jobs/<job-id>/webp-frames/000001.webp
renders/<renderer-version>/q85/2048/<hash-prefix>/<render-hash>.webp
```

Use lifecycle rules:

- `jobs/`: expire after 1-3 days.
- `renders/`: expire after 30 days initially.
- Abort incomplete multipart uploads after one day.
- If bucket versioning is enabled, explicitly expire noncurrent versions and
  delete markers. A non-versioned ephemeral output bucket is simpler.

S3 Lifecycle expiration is based on object age, rounded to midnight UTC, and
processed asynchronously. It is not an exact per-second TTL.

Final WebP object headers:

```text
Content-Type: image/webp
Content-Disposition: inline
Cache-Control: public, max-age=86400
```

The content-addressed URL is immutable while it exists. Keep the CloudFront
cache TTL no longer than the desired period for which a deleted S3 result may
remain visible at an edge.

### Source asset bootstrap and versioning

Add a repeatable administration script that uploads the local source assets;
do not make deployment depend on an undocumented manual bucket copy. It should:

1. Walk `bot/assets/swf_item_index/swf_assets/` without flattening or changing
   path case.
2. Validate every candidate as an SWF rather than an HTML/error response.
3. Calculate SHA-256 and size for every object.
4. Upload missing immutable objects under a versioned prefix.
5. Upload `item_db.json` and a manifest that maps normalized game path to S3
   key, hash, byte size, and asset-dataset version.
6. Verify the uploaded manifest by sampling and hashing downloaded objects.
7. Print counts, bytes, skipped files, failures, and the exact version for the
   stack configuration.

Never apply the `jobs/` or `renders/` expiration policies to the source bucket.
Updating the corpus creates a new dataset version; it must not mutate a version
already referenced by a render hash.

## CloudFront and Discord-facing URLs

Keep S3 Block Public Access enabled. Configure CloudFront Origin Access Control
so only the distribution can read final render objects. Viewer access through
CloudFront may be public because AQW appearances are public and object keys are
unguessable hashes.

The initial URL will look like:

```text
https://d123example.cloudfront.net/renders/v3/q85/2048/7f/<sha256>.webp
```

A custom domain can be added later:

```text
https://chars.example.com/renders/v3/q85/2048/7f/<sha256>.webp
```

Do not use direct public S3 objects. Do not use expiring S3 presigned URLs for
the normal Discord result: they are long, expire, and can break old embeds when
Discord fetches the image again. API Gateway is unnecessary because neither
job submission nor image delivery requires a public application API.

Discord supports HTTP(S) embed image URLs and animated image media. Still run
an end-to-end test with a real CloudFront-hosted Q85 animated WebP on desktop
and mobile before launch. If Discord refuses to animate or proxy a very large
2048 result, use a static PNG thumbnail in the embed plus an Open Animation
link. Do not silently reduce fidelity without recording the decision.

## Discord command lifecycle

The first command shape should be deliberately small:

```text
/char username:<required>
```

Add one optional item override only when the current local override behavior is
ready to expose safely. The current compositor supports one override source per
run. Do not imply that arbitrary simultaneous armor, helm, weapon, cape, and
ground overrides already work.

Command behavior:

1. Call `await interaction.response.defer()` immediately. Discord requires the
   initial acknowledgement within three seconds.
2. Validate and normalize the username and options.
3. Generate a stable `job_id`; the Discord interaction ID is a useful
   idempotency component.
4. Atomically reserve the user's generation slot and create the job record.
5. Send the job request to SQS.
6. If enqueue fails, perform the idempotent slot release and report the error.
7. Keep an in-memory mapping from `job_id` to the interaction object while the
   bot process remains alive.
8. A lightweight background task long-polls `char-render-results`.
9. On success, use `embed.set_image(url=result_url)` and edit the original
   deferred response while the interaction token is valid.
10. If the interaction is older than 15 minutes or the bot restarted, send a
    normal message in `channel_id` mentioning the requesting user.
11. On failure, edit/send a concise failure message with the job ID; do not
    expose internal stack traces.
12. Acknowledge/delete the result queue message only after notification is
    recorded or safely made idempotent.

Do not put the short-lived Discord interaction token into SQS or Step Functions
logs. The result message should contain `job_id`, `channel_id`, and `user_id`.
The bot can use its normal bot token for the fallback message.

Log `/char` use consistently with the bot's existing command/search history,
including job ID, guild ID, user ID, cache status, frame count, outcome, and
duration. Do not log secrets or raw AWS credentials.

## Job request contract

Use versioned JSON contracts and reject unsupported schema versions. The SQS
body should remain small and contain no binary data.

Example request:

```json
{
  "schema_version": 1,
  "job_id": "8d1c70fd-6c7a-4abc-a539-014575b09078",
  "created_at": "2026-08-28T20:00:00Z",
  "discord": {
    "user_id": "123456789012345678",
    "guild_id": "234567890123456789",
    "channel_id": "345678901234567890"
  },
  "render": {
    "username": "Soltina",
    "base_items": false,
    "show_hidden": false,
    "facing": "right",
    "override": null,
    "complete_loop": true,
    "max_frames": 360,
    "subframe_start": 1,
    "zoom": 2.0,
    "raster_size": 2048,
    "output_size": 1024,
    "padding": 0,
    "webp_quality": 85,
    "webp_method": 4
  }
}
```

The bot should not accept arbitrary values for infrastructure-sensitive fields
in v1. Populate them from server configuration. Only username and explicitly
supported safe command options come from the user.

## Launcher and SQS behavior

Configure the job queue event source with batch size one. The Launcher calls
`StartExecution` on the Standard state machine using `job_id` as the execution
name. A successful `StartExecution` allows the SQS event to be acknowledged.
An error causes normal SQS retry/redrive behavior.

Handle duplicate delivery deliberately. Standard Workflow starts are
idempotent while the same named execution with the same input is running, but
a replay after that execution has closed can report `ExecutionAlreadyExists`.
Verify that the existing execution belongs to the same job/request and treat
that case as an acknowledged duplicate. Never start a second execution under a
new name merely to make the error disappear.

The workflow is durable after `StartExecution`, so the Launcher must return; it
must not wait for the complete render. Launcher reserved concurrency one only
serializes these short launch calls.

Set the job queue visibility timeout comfortably beyond the Launcher timeout
and batching window. With the proposed 30-second Launcher and no batching
window, start at 180 seconds. Enable the SQS event source only after the state
machine and job-table permissions exist.

All workflow operations must be idempotent because SQS, Lambda, EventBridge,
and Step Functions integrations may deliver or retry work more than once.

## Step Functions workflow

Use a Standard Workflow. The logical state graph is:

```text
Prepare
  -> CacheHit?
       yes -> CompleteCacheHitAndRelease -> EmitResult -> MarkResultEnqueued -> Success
       no  -> RenderMap (compose + rasterize + encode per batch)
                  -> Finalize (mux, publish, CompleteSuccessAndRelease inline,
                     EmitResult, MarkResultEnqueued)
                  -> Success

Any unrecovered rendering error
  -> CompleteFailureAndRelease
  -> EmitFailureResult
  -> MarkResultEnqueued
  -> Fail
```

Each Lambda Task needs retry rules for transient Lambda service errors and
throttling with exponential backoff. Application/rendering errors should have
a small bounded retry count. Each Map iteration retries only its failed batch.

The `Complete*AndRelease` states perform the terminal job update and user-slot
release in one DynamoDB transaction. Result emission happens afterward and has
its own retries. If result-queue emission ultimately fails, the render remains
terminal and its slot remains released; an alarm plus the reconciler must
re-enqueue any terminal job lacking `result_enqueued_at`.

Step Functions `Map` waits until all iterations complete successfully before
entering the next state. That is the synchronization mechanism for the
Finalizer; do not implement polling loops between Lambdas.

The shared animation canvas is computed in Prepare from the raw FFDec export
headers (root dimensions plus the outer zoom/crop matrix), so no intermediate
bounds-reduction state exists. Every render worker rasterizes against that
canvas, which keeps one pixel scale for the whole animation. The canvas uses
the conservative vector-bounds union plus the same filter-glow margin the
composer already applied, rather than an alpha-probe rasterization per frame;
this removes the old double rasterization of every frame.

Keep state payloads small. Pass job IDs, batch ranges, S3 manifest keys, and
small status objects through Step Functions. Store SVGs, PNGs, WebPs, and large
manifests in S3.

## Prepare Lambda

Prepare is the only stage that should need Java/FFDec.

Responsibilities:

1. Load and validate the job record.
2. Fetch the public AQW character response or use an explicitly frozen test
   FlashVars file.
3. Resolve effective cosmetic/base visibility exactly as the local renderer
   does.
4. Validate an optional item override exclusively through trusted item IDs and
   source-asset keys. Reject arbitrary paths and URLs.
5. Resolve each required source SWF from the source bucket. If policy allows a
   missing file to be fetched from AQW, restrict the host and normalized path,
   validate the SWF header, and store it atomically in the source bucket.
6. Record source object versions or SHA-256 hashes.
7. Build the exact ordered symbol requests and layer aliases.
8. Export the required nested SVG states with headless FFDec.
9. Parse and preserve color customization scripts.
10. Detect the complete combined loop using the existing blink-ignore logic.
11. Calculate drift-free frame durations from the SWF frame rate.
12. Calculate the canonical render hash.
13. Check whether the final content-addressed S3 result already exists.
14. On a miss, upload prepared raw part SVGs/rules and a prepare manifest.
15. Produce deterministic frame batches of 15-30 frames.

The canonical render hash must be SHA-256 over canonical JSON containing at
least:

- Renderer/schema version.
- `characterB.swf` hash.
- FFDec/libwebp compatibility version when it affects output.
- Resolved effective appearance fields.
- Source SWF keys and hashes.
- Exported class/link names and weapon holder type.
- Color customization values.
- Visibility/base/cosmetic choices.
- Override identity and hash.
- Facing, zoom, maximum size, padding.
- Loop policy, output cap, and subframe start.
- WebP quality/method and timing policy.

Do not key the final cache by username alone because a player can change their
outfit. The final key must describe the resolved content and settings.

A cache hit can only be known reliably after resolving the current appearance.
Therefore, v1 may briefly reserve a user slot and run Prepare before discovering
a cache hit. Release the slot immediately on the cache-hit branch. A future
short-lived request-to-render alias can optimize this if necessary.

## Map: render workers (compose + rasterize + encode)

Each iteration receives a frame range and the prepare-manifest S3 key. It must:

1. Download the manifest and one compressed part archive per symbol (Prepare
   uploads `jobs/<job-id>/prepare/parts/<part>.tar.gz` instead of one object
   per frame).
2. Import symbols using the same zoom correction as the local renderer.
3. Build the exact full character layer order and transforms.
4. Apply color filters and minimum-stroke preparation, composing each frame
   in memory.
5. Apply the shared canvas viewBox from the prepare manifest, recalibrate
   minimum strokes, and rasterize once with `rsvg-convert` to transparent
   RGBA PNG.
6. Compute the delta rectangle against the preceding global frame.
7. Expand delta x/y offsets to even coordinates as required by animated WebP.
8. Encode the cropped frame with pinned `cwebp` arguments equivalent to:

```text
cwebp -quiet -q 85 -alpha_q 100 -m 4 [crop] input.png -o frame.webp
```

9. Upload each encoded frame and a batch manifest containing frame number,
   frame WebP key, x/y offset, duration, canvas dimensions, and checksum.

The shared canvas viewBox is the union of every frame's transformed vector
bounds, computed in Prepare, plus the composer's conservative filter margin
and configured padding:

```text
left   = min(frame.x)
top    = min(frame.y)
right  = max(frame.x + frame.width)
bottom = max(frame.y + frame.height)
margin = max(width, height) * 0.1 + 2

content_pixels = max(1, output_size - 2 * padding)
units_per_pixel = max(width, height) / content_pixels
padding_units = padding * units_per_pixel
```

The shared viewbox uses `output_size` so padding remains expressed in final
pixels. Each frame is rasterized with longest side `raster_size`, then reduced
with premultiplied-alpha Lanczos filtering to `output_size`. When the two sizes
match, the PNG is passed directly to WebP encoding without a resize/rewrite.

The first frame of a batch still depends on the prior global frame for the
smallest delta. Avoid dependencies between concurrently running batches by
having every non-first batch also compose and rasterize the immediately
preceding frame as a temporary overlap frame. Do not upload that duplicate as
an output frame. This duplicates one render per batch while preserving the
current delta behavior and compression.

Do not pass PNGs or frame WebPs through Step Functions payloads. Keep temporary
PNGs in Lambda `/tmp`; only upload them for debugging or when a retry design
requires them. Upload the much smaller encoded frame WebPs for Finalizer input.

## Finalizer Lambda

Finalizer runs only after every render batch succeeds. It must:

1. Read all render batch manifests.
2. Verify exactly one record exists for every expected frame number.
3. Verify canvas dimensions, duration list, hashes, and frame ordering.
4. Download the already-encoded frame WebPs.
5. Run a single ordered `webpmux` command equivalent to the local pipeline,
   with loop count zero and transparent background.
6. Validate the completed animation by reopening it and checking frame count,
   canvas, durations, loop metadata, transparency, and nonzero file size.
7. Upload atomically to the content-addressed final result key with the required
   HTTP metadata.
8. Complete the job inline: write final size, durations, renderer version, and
   result URL to the job record, release the user slot in the same terminal
   DynamoDB transition, and publish the result-queue message.

`webpmux` does not recompress pixels. The expensive `cwebp` conversion occurs
inside the render workers. Finalizer is primarily ordered file I/O plus
container assembly; the inline completion avoids a trailing Lambda state
transition per render. The separate Complete Lambda remains for the cache-hit
branch and the terminal failure path.

Do not publish a partially written final key. Write to a job-specific temporary
key, validate, then copy/promote it to the immutable render key and remove the
temporary object.

## DynamoDB user limits and job state

Use the Discord user ID, not username, as the global quota identity. A user
invoking the bot in multiple guilds still has one shared limit. Make the limit
configurable; start with `2` unless the owner chooses another value.

Queued jobs count as active. Acquire the slot before SQS enqueue so a user
cannot fill the queue with unlimited work.

A single-table design may use:

```text
PK=USER#<discord-user-id>, SK=COUNTER
  active_count
  updated_at

PK=JOB#<job-id>, SK=META
  user_id
  guild_id
  channel_id
  status
  slot_released
  created_at
  updated_at
  execution_arn
  render_hash
  result_url
  result_enqueued_at
  notification_delivered_at
  expires_at
```

Optionally add a GSI with `USER#<id>` and creation time to list a user's recent
jobs.

Acquire with one `TransactWriteItems` call:

1. Put the unique job record with a condition that it does not exist.
2. Update the user counter with a condition equivalent to
   `attribute_not_exists(active_count) OR active_count < :limit`, then add one.

If the condition fails, edit the Discord response with:

```text
You already have X character generations in progress.
Wait for one to finish before starting another.
```

Release with one transaction:

1. Change the job to a terminal status and set `slot_released=true`, conditioned
   on `slot_released` still being false/missing.
2. Decrement the user's counter, conditioned on it being positive.

This makes release idempotent: Step Functions retries, EventBridge cleanup, and
the periodic reconciler cannot decrement twice.

Every normal success, cache hit, and caught failure path must release. Add an
EventBridge rule for Step Functions `SUCCEEDED`, `FAILED`, `TIMED_OUT`, and
`ABORTED` status events that invokes the same idempotent cleanup. EventBridge
status delivery is best effort, so also run a scheduled reconciliation Lambda
that inspects stale nonterminal jobs and their Step Functions execution status.
It must also handle stale `QUEUED` jobs with no execution ARN, such as an SQS
message that exhausted Launcher retries, and terminal jobs missing
`result_enqueued_at`.

DynamoDB TTL may delete old terminal job records, but TTL deletion alone must
not be relied upon to repair a counter because deleting a job item does not
atomically decrement its user counter.

Concurrency limiting is separate from rate limiting. Once a job finishes, the
user may start another immediately. Add a per-hour/day rate limiter only if
abuse requires it.

## Status and result contracts

Suggested job statuses:

```text
QUEUED
PREPARING
FINALIZING
CACHE_HIT
SUCCEEDED
FAILED
TIMED_OUT
ABORTED
```

Example result queue message:

```json
{
  "schema_version": 1,
  "job_id": "8d1c70fd-6c7a-4abc-a539-014575b09078",
  "status": "SUCCEEDED",
  "discord": {
    "user_id": "123456789012345678",
    "guild_id": "234567890123456789",
    "channel_id": "345678901234567890"
  },
  "result": {
    "url": "https://d123example.cloudfront.net/renders/v3/q85/2048/7f/7f8a.webp",
    "frame_count": 360,
    "width": 2048,
    "height": 1664,
    "duration_ms": 15000,
    "bytes": 98765432,
    "cache_hit": false
  }
}
```

Failure messages should contain a stable public error code and safe user-facing
message. Keep stack traces and detailed FFDec diagnostics only in private logs.

## Cache and duplicate work

The required v1 cache is the immutable content-addressed final S3 key. Prepare
checks it after resolving the current appearance.

Two identical jobs can still enter Prepare simultaneously before either final
object exists. This is likely rare. After the base system works, add a
DynamoDB render-lock item keyed by the canonical render hash:

```text
PK=RENDER#<sha256>, SK=LOCK
owner_job_id
status
lease_expires_at
result_url
```

Use a conditional lease. The owner renders; followers wait in Step Functions
without holding Lambda compute and then reuse the owner's result. Include lease
expiry and ownership tokens so a crashed owner cannot block the hash forever.

## Idempotency and failure handling

- Use `job_id` as the Step Functions Standard execution name so duplicate
  launcher delivery does not create a second execution.
- Every S3 key written by a stage is deterministic for job, batch, and frame.
- Write manifests only after their referenced objects are successfully stored.
- Validate existing objects before treating a retried stage as complete.
- A batch retry may overwrite only its own job-scoped keys.
- Final promotion to the content-addressed result is atomic from the consumer's
  perspective.
- The slot-release operation must be idempotent.
- Result notification must be idempotent so duplicate result queue messages do
  not create duplicate Discord messages.
- Mark `result_enqueued_at` only after SQS accepts the result message. Mark
  `notification_delivered_at` after the bot successfully edits or sends the
  Discord response. The reconciler repairs missing enqueue records; the bot
  safely acknowledges already-delivered duplicate messages.
- Configure DLQs for both SQS queues and alarms whenever a message appears.
- Configure an overall Step Functions execution timeout. A normal Discord
  interaction token lasts 15 minutes, but the fallback channel-message path
  permits the workflow to finish later. Each individual Lambda remains limited
  to 15 minutes.
- Sanitize exception messages before sending anything to Discord.

## Security and IAM

The Hetzner bot's AWS principal should have only:

- The required DynamoDB transaction/read operations on the render table.
- `sqs:SendMessage` on the job queue.
- `sqs:ReceiveMessage`, `sqs:DeleteMessage`, and visibility operations on the
  result queue.
- No Lambda invocation, Step Functions control, ECR, broad S3, or CloudFront
  administration permissions.

Store Hetzner credentials in its protected systemd environment file or an
equivalent secret store. Never commit credentials to `.env`, source, logs, SQS
messages, or this repository.

Lambda roles should be separate by stage and restricted to their S3 prefixes
and required DynamoDB/SQS/Step Functions operations. CloudFront's bucket policy
must allow reads only from the intended distribution through OAC.

Input restrictions:

- Normalize AQW usernames and impose a small length/character limit.
- Allow source downloads only from the official AQW host and normalized
  `gamefiles` paths.
- Reject traversal, redirects to unexpected hosts, HTML error bodies, invalid
  SWF headers, and excessive response sizes.
- Allow overrides only through trusted item IDs/S3 keys derived from the item
  database.
- Enforce server-side `max_size`, `max_frames`, quality, and batch limits.
- Do not let Discord input control an arbitrary S3 key, shell argument, URL,
  file path, FFDec class, or command.
- Use argument arrays for subprocesses; never concatenate user values into a
  shell command.

## Configuration

Add documented bot variables to `.env.example` without committing real
values. Suggested names:

```text
CHAR_RENDER_ENABLED=false
AWS_REGION=us-west-2
CHAR_RENDER_JOB_QUEUE_URL=
CHAR_RENDER_RESULT_QUEUE_URL=
CHAR_RENDER_JOB_TABLE=
CHAR_RENDER_ENABLED_PARAMETER=
CHAR_RENDER_MAX_ACTIVE_PARAMETER=
CHAR_RENDER_RESULT_POLL_SECONDS=20
```

The maximum-active value is emitted by CDK as an SSM parameter from the typed
environment tuning file. `CHAR_RENDER_MAX_ACTIVE_PER_USER` remains an optional
bot-side emergency override and should normally be blank.

The Lambda/stack configuration should include:

```text
CHAR_RENDER_SCHEMA_VERSION=1
CHAR_RENDERER_VERSION=v3
CHAR_RENDER_DEFAULT_RASTER_SIZE=2048
CHAR_RENDER_DEFAULT_OUTPUT_SIZE=2048
CHAR_RENDER_ZOOM=2
CHAR_RENDER_PADDING=0
CHAR_RENDER_MAX_FRAMES=360
CHAR_RENDER_BATCH_SIZE=30
CHAR_RENDER_MAP_CONCURRENCY=4
CHAR_RENDER_WEBP_QUALITY=85
CHAR_RENDER_WEBP_METHOD=4
CHAR_RENDER_RESULT_RETENTION_DAYS=30
CHAR_RENDER_JOB_RETENTION_DAYS=2
CHAR_RENDER_PUBLIC_BASE_URL=https://d123example.cloudfront.net
```

Use infrastructure references/parameters rather than copying queue URLs,
bucket names, and ARNs manually where possible.

The bot currently does not depend on `boto3` explicitly. Add only the scoped
AWS client dependency needed by the bot. Do not install the repository's full
machine-learning requirements into Lambda images; define small renderer-specific
dependency locks.

## Observability

Emit structured JSON logs with at least:

```text
job_id
render_hash
discord_user_id
stage
batch
frame_start/frame_end
cold_start
cache_hit
frame_count
input/output bytes
duration_ms
ffdec_ms
render_ms
cwebp_ms
webpmux_ms
s3_ms
status/error_code
```

Do not include Discord tokens, AWS credentials, or full private payloads.

Add CloudWatch metrics/alarms for:

- Job queue age and DLQ count.
- Result queue age and DLQ count.
- Step Functions failed, timed-out, and aborted executions.
- Lambda errors, throttles, duration, memory use, and timeouts per stage.
- Render total and per-stage latency percentiles.
- Cache-hit rate.
- Frames and bytes produced.
- User-limit rejections.
- S3 result/intermediate storage.
- Estimated AWS spend/budget threshold.

CloudWatch's Lambda `Init Duration` measures the platform/runtime cold start.
Log Java/FFDec process startup separately because the current renderer starts
Java inside the invocation, so that cost occurs on warm invocations too.

## Cost expectations

There is no charge merely for configuring worker reserved concurrency eight or
Map maximum concurrency eight. Actual Lambda GB-seconds, requests, extra
ephemeral storage above the included amount, S3 operations/storage, CloudFront
transfer, and Step Functions transitions are billed.

Ideal parallelism preserves total Lambda compute:

```text
1 worker * 4 GB * 800 s = 3,200 GB-s
5 workers * 4 GB * 160 s = 3,200 GB-s
```

The distributed version is slightly more expensive because of per-worker cold
starts, duplicated dependencies/downloads, S3 intermediate operations,
stragglers, and orchestration. It should be much faster in wall-clock time.

DynamoDB is negligible at expected volume. Small transactional user/job
records consume roughly eight write request units over acquire and release.
At 1,000 generations per month that is only thousands of request units, while
the table remains far below the free storage allowance.

At local 1024-ish output dimensions, the existing 360-frame Soltina Q85 WebP
benchmark was approximately 24.8 MiB and about 9.3 seconds for WebP encoding
alone. Exact lossless was approximately 62.2 MiB and 29 seconds. A 2048 render
will be materially larger; measure actual S3/CloudFront transfer and Discord
behavior before launch.

## Suggested code organization

Do not copy the monolithic script into every handler. Extract tested pure and
stage-level APIs while retaining the CLI as a local wrapper.

The repository begins with this layout and should grow within it:

```text
bin/                              TypeScript CDK entry point
lib/                              stacks, constructs, and environment config
services/renderer/
  Dockerfile
  pyproject.toml
  src/aqw_char_renderer/
    contracts.py
    appearance.py
    hashing.py
    manifests.py
    storage.py
    subprocesses.py
    stages/
    handlers/
  tests/
test/                             CDK tests
docs/                             architecture and operations
```

The exact names may change, but keep rendering logic independent from AWS
handler parsing so it remains runnable and testable locally.

Add the Discord command and result listener to focused modules rather than
making `discord_bot.py` substantially more monolithic if a clean extraction is
possible.

## Owner inputs needed before production

Implementation can begin with the defaults in this document. Before a public
deployment, confirm:

- Production AWS account or accounts and Regions. Development is fixed to
  account `538522204887` in `us-west-2`.
- Tester guild ID or IDs and the date for global command sync.
- Per-user active-job limit; use two initially if unspecified.
- Generated CloudFront hostname versus a custom character-render domain.
- Maximum acceptable final WebP size and end-to-end render latency after real
  2048-pixel benchmarks.
- Who owns refreshing `item_db.json`, the source SWF bucket, and pinned
  `characterB.swf`/FFDec versions.

None of these choices should block the local refactor or a private staging
stack.

## Validation/deployment order

### Phase 1: profile and refactor locally (implemented)

1. Preserve the existing monolithic command as a reference.
2. Record per-stage timings for Soltina at 512, 1024, and 2048.
3. Extract versioned manifest models and pure render-stage functions.
4. Build a local coordinator that runs the same two Maps sequentially or with a
   local executor and produces the same final WebP.
5. Verify output before introducing AWS.

### Phase 2: container compatibility (image defined; Linux build pending)

1. Build the pinned Linux renderer container.
2. Run the complete saved Soltina fixture without network access except local
   mock/S3 equivalents.
3. Confirm headless FFDec, librsvg, Pillow, cwebp, and webpmux behavior.
4. Compare decoded output and timing with macOS reference output.

### Phase 3: AWS staging workflow (CDK implemented; deployment pending)

1. Deploy private staging buckets, queues, DynamoDB, Lambdas, and Step
   Functions without CloudFront or Discord.
2. Run saved fixtures through SQS.
3. Test retries, one failed batch, timeout, abort, idempotent release, and DLQs.
4. Tune batch size, Lambda memory, local worker count, and Map concurrency.

### Phase 4: delivery and Discord (code implemented; live integration pending)

1. Add CloudFront OAC and lifecycle rules.
2. Test a real animated WebP URL in Discord desktop and mobile.
3. Add the result-queue listener and `/char` behind
   `CHAR_RENDER_ENABLED=false`.
4. Enable only in the configured tester guild first.
5. Add dashboards, alarms, budget, and operational documentation.

### Phase 5: optional optimization

- Add in-flight render-hash deduplication.
- Split the shared container into lean stage-specific images if cold starts or
  image size matter.
- Reuse warm `/tmp` source assets.
- The shared canvas is derived from vector bounds; if glow/filter clipping is
  ever observed, evaluate an alpha-trim pass on rendered PNGs rather than
  restoring the removed per-frame probe rasterization.
- Add scheduled or manual cleanup/reconciliation tooling.
- Add safe item overrides to the Discord interface.

## Test plan

### Unit tests

- Request/manifest schema validation and version rejection.
- Canonical render hash stability and sensitivity to every fidelity setting.
- Deterministic batch partitioning with no missing/duplicate frames.
- Complete-loop frame duration sum without cumulative rounding drift.
- Shared-canvas union math, padding, and negative coordinates.
- Applying shared SVG geometry and recalibrating strokes.
- Delta bounds at batch boundaries and even x/y expansion.
- Ordered final mux manifest validation.
- Cache-hit paths.
- Atomic user-limit acquire under simultaneous requests.
- Idempotent success/failure/abort release.
- SQS duplicate launcher delivery.
- Result-notification duplicate handling.
- Unsafe username/path/URL/SWF rejection.

### Local integration fixtures

- Soltina saved FlashVars: 360-frame complete loop.
- TDNQ with Cursed Claws: verify the offhand weapon remains in front of the
  cape according to the corrected display order.
- A character with hidden helm/cape and cosmetic overrides.
- Color-custom armor/helm/cape/weapon.
- A no-animation or one-frame character.
- An intentionally missing or invalid SWF.
- A frame with changing bounds to prove there is no jitter.

For lossy Q85 output, do not require byte equality with a different encoder
build. Decode and verify:

- Frame count and order.
- Canvas dimensions.
- Frame durations and complete loop length.
- Transparent background.
- No frame-to-frame positional jitter.
- Expected display order.
- Pixel/perceptual similarity against the local reference.

### AWS staging tests

- Cold and warm executions.
- Two jobs overlapping after Prepare while global worker concurrency remains
  bounded.
- A single user hitting the configured active-job limit.
- Multiple users sharing worker capacity.
- Cache hit and simultaneous identical cache miss.
- Batch Lambda retry without duplicate output.
- Workflow failure, timeout, and manual abort all release the user slot.
- Bot restart before result delivery falls back to a normal channel message.
- S3 intermediate and result lifecycle behavior.
- CloudFront URL access while direct S3 access remains denied.
- Real Discord animated WebP rendering at expected file sizes.

## Acceptance criteria

The first production release is complete only when:

- `/char username:<name>` acknowledges within three seconds.
- Hetzner does not execute Java, FFDec, librsvg, or WebP encoding.
- The distributed output matches local rendering fidelity and animation timing.
- Every output frame uses one shared canvas with no jitter.
- Q85 and 2048 maximum dimension are configurable and recorded in the cache
  key.
- Parallel frame batches materially reduce wall-clock rendering time.
- No Lambda exceeds its 900-second timeout under the maximum supported input.
- The result is stored privately and displayed through a CloudFront HTTPS URL.
- A user cannot exceed the configured queued/running job count even with
  simultaneous Discord requests.
- Success, failure, timeout, abort, enqueue failure, and duplicate delivery do
  not leak a user slot.
- Results and intermediates expire according to their prefix policies.
- Secrets do not appear in Git, Step Functions input, SQS messages, or logs.
- A staging run and tester-guild run have been completed before global command
  sync.

## Local reference command

Use the existing saved Soltina appearance as a reference for the desired
2048-pixel Q85 output. Run this from the sibling source repository:

```bash
cd ../aq-image-search
./venv/bin/python -X faulthandler pipeline/render_swf_character_svg.py Soltina \
  --flashvars-json render_outputs/character_svg/soltina/source/soltina_flashvars.json \
  --offline \
  --complete-loop \
  --max-frames 360 \
  --subframe-start 1 \
  --zoom 2 \
  --max-size 2048 \
  --padding 0 \
  --workers 4 \
  --webp-encoder auto \
  --webp-method 4 \
  --webp-lossy-quality 85 \
  --output render_outputs/character_svg/soltina/frames_2048/soltina.svg \
  --preview-png render_outputs/character_svg/soltina/frames_2048/soltina.png \
  --animation-webp render_outputs/character_svg/soltina/soltina-2048-q85.webp \
  2>&1 | tee logs/render-soltina-2048-q85.log
```

## Primary platform references

- [Discord interaction response deadlines](https://docs.discord.com/developers/interactions/receiving-and-responding)
- [Discord embed image URLs](https://docs.discord.com/developers/resources/message)
- [Lambda quotas and 15-minute timeout](https://docs.aws.amazon.com/lambda/latest/dg/gettingstarted-limits.html)
- [Lambda container image requirements](https://docs.aws.amazon.com/lambda/latest/dg/images-create.html)
- [Lambda execution environment and cold starts](https://docs.aws.amazon.com/lambda/latest/dg/lambda-runtime-environment.html)
- [Lambda reserved versus provisioned concurrency](https://docs.aws.amazon.com/lambda/latest/dg/lambda-concurrency.html)
- [Step Functions Inline Map](https://docs.aws.amazon.com/step-functions/latest/dg/state-map-inline.html)
- [Step Functions service integrations](https://docs.aws.amazon.com/step-functions/latest/dg/integrate-services.html)
- [Step Functions EventBridge status events](https://docs.aws.amazon.com/step-functions/latest/dg/eventbridge-integration.html)
- [Using Lambda with SQS](https://docs.aws.amazon.com/lambda/latest/dg/with-sqs.html)
- [DynamoDB conditional writes and counters](https://docs.aws.amazon.com/amazondynamodb/latest/developerguide/WorkingWithItems.html)
- [DynamoDB transactions](https://docs.aws.amazon.com/amazondynamodb/latest/developerguide/transaction-apis.html)
- [CloudFront Origin Access Control for S3](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/private-content-restricting-access-to-s3.html)
- [S3 Lifecycle rules](https://docs.aws.amazon.com/AmazonS3/latest/userguide/intro-lifecycle-rules.html)
- [Lambda pricing](https://aws.amazon.com/lambda/pricing/)
- [Step Functions pricing](https://aws.amazon.com/step-functions/pricing/)
- [DynamoDB pricing](https://aws.amazon.com/dynamodb/pricing/)
