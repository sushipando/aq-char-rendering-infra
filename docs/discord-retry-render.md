# Discord retry-render and render statistics

`/retry-render` is registered only in `TEST_GUILD_ID` and also checks the guild at invocation time. It accepts a job UUID from the configured job table. Any member who can use commands in that test guild can retry a job; it is not restricted to the original job owner.

A retry creates a fresh job through normal Discord quota/admission and result delivery, using the invoking user's current interaction. It inherits the saved appearance, settings, item override, fan-out modes and cache flags. It does not look up the character's current equipment. Omitted options inherit; explicit `false` and `0` override. To force new rendering instead of a final-cache hit, set `render_cache:false`.

Examples in Discord:

```text
/retry-render job:6fd4e305-3ff1-4c5f-a253-65b7e5530242 render_cache:false
/retry-render job:6fd4e305-3ff1-4c5f-a253-65b7e5530242 output_format:avif quality:70 render_cache:false
```

The command has 25 options, Discord's limit. Common settings have individual options. `overrides` accepts a JSON object for remaining inputs; explicit named options take precedence over JSON. Supported sections are `render`, `cache`, `appearance`, `bounds_mode` and `component_raster_mode`. Job identity and Discord destination cannot be overridden.

Example `overrides` values:

```json
{"render":{"avif_speed":9,"avif_quality":80},"cache":{"render":false}}
```

```json
{"render":{"override":null},"appearance":{"intColorHair":"16711680"}}
```

```json
{"cache":{"render":false,"animation":false,"vectors":false,"bounds":false,"components":false}}
```

`render.override` is replaced as a whole when supplied in JSON; `null` clears it. Appearance fields merge into the saved snapshot and must be strings. A username override changes the request name, not the equipment snapshot. All render options still pass normal input validation. Existing jobs with raw AVIF handoffs are retried using zstd. Discord no longer offers or accepts raw AVIF intermediates; low-level pipeline compatibility and the operator raw/zstd benchmark remain available.

## Bot configuration and owner deployment

The bot first loads the stored request from DynamoDB, which the existing bot policy already permits. For jobs without appearance in that request it reads `jobs/<id>/prepare/input.json`, falling back to `manifest.json`. Missing snapshots fail instead of silently rendering current equipment.

Configure the bot with:

```text
CHAR_RENDER_WORK_BUCKET=<WorkResultBucketName output>
```

The CDK bot managed policy now allows `s3:GetObject` on those two prepare snapshot paths in the work bucket. Deploy this policy change for snapshot fallback, and update/restart the bot so its test-guild commands synchronize. Deploy the updated Rust pipeline image for new embedded render statistics. ARM image builds and deployments remain owner-run.

## View Render Info

Both final WebP and AVIF files embed these additional XMP fields:

- `aqw:renderTimeMs`: elapsed wall time from the beginning of PrepareResolve through completed encoding. This includes intervening pipeline stages, but excludes initial queue/startup time, final upload, job completion and Discord delivery. It is not animation duration or summed CPU time.
- `aqw:fileSizeBytes`: exact final container size, including XMP itself.
- `aqw:renderTimeScope`: `prepare-to-encoded-file`.

The **Render Info** section displays job ID, render time in seconds with its scope, and file size in MiB plus exact bytes. Old files remain readable and show `N/A` for absent statistics. Cached files retain the original generating job's statistics; they do not report the current cache lookup latency.

The fields reserve fixed-width storage in XMP. After encoding, the finalizer locates its exact packet and fills the fields in place, without changing file length, container offsets or image data. No second encode is required. Metadata policy v3 changes final cache identity; upstream render caches are still reusable. Previously delivered files are not modified.

## Validation and smoke checks

Local tests cover merging and validation, guild registration/runtime restriction, current-user admission, saved-appearance fallback, bot parsing/display for both formats, and the actual Rust WebP/AVIF output path with exact-size assertions. No AWS renders or ARM builds are needed for these local checks.

After your deployment:

1. Retry the reference job as WebP with `render_cache:false`. Use **Apps → View Render Info** on the result; verify its new job ID, size and render time.
2. Retry it as AVIF with `render_cache:false`; repeat the info check. Only one AVIF format option should be visible.
3. Retry with a changed quality or item override and confirm omitted inputs are preserved. Retry an older CLI job to exercise snapshot fallback if its saved request lacks appearance.
4. Confirm `/retry-render` is absent from other guilds. A cache hit should retain the original file's generating ID and statistics.

## Result delivery

Both commands show a short queued status with job IDs. Failed render results include the failed job UUID; queue submission failures also include the new UUID once one has been created.

Successful WebP/AVIF results with a reported size below 10,000,000 bytes are downloaded with a strict byte limit and sent as a Discord attachment. Original encoded bytes and embedded metadata are preserved. The bot also respects a smaller guild upload limit when available. Discord controls whether a given format previews inline.

Larger files, results without a reported size, failed downloads and rejected uploads fall back to the original render URL. Upload rejection first retries the same interaction with the URL; expired interactions use channel delivery. Mention suppression applies to both paths. No Discord upload or live render was performed during local validation; attachment, failure and fallback behavior is covered by mocked delivery tests.

The Discord option is named `lossless` for both commands (the wire/API field remains `webp_lossless` for compatibility). For lossless AVIF use `/render username:YourName output_format:avif lossless:true`. Successful attachment replies contain only the uploaded image, with no `Job:` text; its job ID remains available through View Render Info. Queued and failed replies still show job IDs.
