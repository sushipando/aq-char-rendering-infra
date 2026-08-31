# AQW Character Rendering Infrastructure

This repository owns the distributed `/char` rendering system: AWS CDK
infrastructure, Step Functions orchestration, Python rendering services,
container definitions, tests, and operational documentation. Rendering uses
FFDec only; it does not use AIR or Ruffle.

Nothing has been deployed from this working tree yet. `cdk synth` is read-only,
but `cdk bootstrap` and `cdk deploy` create billable AWS resources.

## Development environment

| Setting | Value |
| --- | --- |
| AWS account | `538522204887` (`aqw-char-dev`) |
| AWS Region | `us-west-2` |
| AWS CLI profile | `aqw-char-dev` |
| CDK language | TypeScript |
| Renderer language | Python 3.13, managed with `uv` |

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

Install and validate all Python workspace packages:

```bash
uv sync --all-packages
uv run --package aqw-char-renderer pytest services/renderer/tests
uv run --package aqw-char-renderer ruff check services/renderer
```

Docker is required before deployment because every renderer Lambda uses the
same pinned Linux container image. The image includes FFDec 26.2.1, Java 21,
`rsvg-convert`, `cwebp`, and `webpmux`.

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

The launcher hydrates sparse requests from this configuration. `/char-hd`
exposes only a named final-size choice; the bot derives `raster_size` as
exactly twice `output_size`. Matching sizes from other clients still skip
resampling, while larger rasters are downsampled once before WebP encoding.

## Architecture

```text
Discord bot -> DynamoDB admission transaction -> job SQS -> launcher
  -> Step Functions Standard
      -> Prepare (resolve character, FFDec export, loop/cache detection,
         shared-canvas computation, per-part frame archives)
      -> parallel render batches (compose in memory -> rasterize once
         against the shared canvas -> optional downsample -> WebP encode)
      -> final WebP mux, immutable promotion, and inline job completion
      -> result SQS -> Discord bot

CloudFront -> private S3 /renders/ objects
```

The bulk SWF corpus is checksummed and immutable. If a live character refers
to a legitimate staff/legacy asset absent from that corpus, Prepare can fetch
only from `https://game.aq.com/game/gamefiles/`, validate the SWF header, and
atomically preserve the first copy in the private versioned source bucket.

## Deployment sequence

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
test/                 CDK unit tests
docs/                 architecture and operating documentation
```

No AWS credentials, SSO cache files, downloaded SWFs, render frames, or final
animations belong in Git.
