# AWS-only source fetching

Updated September 7, 2026. This supersedes the earlier bot/CLI prefetch design.
The standalone ARM64 fetch Lambda has now been deployed and tested in AWS with
Bright Data credentials. All five invocations passed. See
[the AWS test results](source-fetch-aws-test-2026-09-07.md). The main rendering
workflow and bot have not been deployed as part of this standalone test.

## Current flow

```text
Discord or CLI: username + render options (+ previous job ID for retries)
  → SQS/admission
  → Step Functions: FetchSources (Python, curl_cffi, Bright Data)
      → fetch public character data
      → resolve equipment, item overrides, and animated background
      → reuse source S3 cache or download and cache missing SWFs
      → save appearance and fetched request in work S3
  → PrepareResolve (Rust, S3 reads only)
  → existing export/raster/compose stages
```

`FetchSources` is the first task inside the protected workflow. An exception,
proxy failure, invalid response, or Lambda timeout goes through
`CompleteFailedJob` and then `RenderFailed`, releasing the user slot and producing
the normal failure notification with the job ID. We no longer reject hidden or
unavailable characters in the bot before job creation.

The bot, `submit_render.py`, smoke submissions, CLI restarts and new benchmark
candidates no longer fetch AQW or upload source assets. Their submissions contain
no populated appearance snapshot. Retries use `source_job_id` and optional
`appearance_overrides`; snapshot loading happens inside AWS. Historical explicit
appearance payloads remain accepted for compatibility.

New fetch attempts save `jobs/<job>/fetch/appearance.json` before asset downloads,
so a retry can preserve the appearance even if a later SWF fetch fails. A completed
step additionally saves `fetch/request.json` and `fetch/sources.json`. Internal
retries reuse the completed acquisition. New jobs fetch fresh character data;
retries prefer the previous job's snapshot. If none was ever captured, they fetch
the character's current data. Older snapshots can also be recovered from prepare
files or the original DDB request (integrated workflow only for the DDB fallback).

The Rust prepare stage has **no AQW HTTP fallback**. A missing source at that
point is an error in acquisition/planning and fails clearly instead of making a
request with a different fingerprint.

## HTTP session and caching

The Python step uses pinned `curl_cffi==0.15.0`, `impersonate="chrome136"`, and
one acquisition session for character data, then up to six concurrent source
workers. Each worker has its own curl client, a copy of the page's cookies
(including domain/path restrictions), the same sticky proxy identity, and the
same browser profile. Workers do not share mutable curl handles; cookies set by
one asset response are not merged into other workers. It does not execute JavaScript or solve challenges.
See [curl_cffi documentation](https://curl-cffi.readthedocs.io/en/stable/index.html).

The proxy username is:

```text
<base username>-session-<random attempt UUID without hyphens>-const
```

Every acquisition attempt gets a new random session identifier, including retries. A provider may still select a
previously used IP. `-const` asks Bright Data to fail if the peer/session is lost,
rather than silently changing IP mid-attempt. Idle expiry still applies. See
[Bright Data session controls](https://docs.brightdata.com/api-reference/proxy/rotate_ips).

Manifest and dynamic S3 cache hits avoid origin SWF downloads. Cache inspection
uses HEAD requests; cached SWFs are not downloaded into Python merely to seed
them again. Missing sources are validated and conditionally created with SHA-256
metadata. Background hashes are checked against the packaged source catalog.
The first step reads the item database when an item override requires it.

TLS verification is enabled, redirects are limited to the two official AQW HTTPS
origins, and decompressed response bytes are bounded. Proxy failures do not fall
back to a direct request. Proxy credentials only go to Bright Data. AWS SDK calls
use their own clients. Cookies stay in Lambda memory and are not stored in job
payloads, render metadata, or logs.

## Recovery and concurrent downloads (implemented, awaiting deployment)

The acquisition step makes up to **three total attempts**, with 1- and 2-second
backoffs. Transport failures and HTTP 403/408/429/5xx restart the acquisition with
fresh cookies and a new Bright Data session identifier. It requests the character
page again and replans assets; successful immutable S3 objects remain cache hits.
For `/retry-render` or explicit historical snapshots, recovery refreshes the page
for the network session but preserves the requested snapshot and overrides.
Completed acquisitions still return their saved request without contacting AQW.

Up to **six workers** process distinct planned paths concurrently. Cache hits do
not open a curl client. Missing assets download, validate, and upload independently.
The coordinator waits for every worker, including successful uploads, before
starting another attempt. A failed attempt never publishes `fetch/request.json`.
Permanent errors (404, bad configuration, checksum mismatch, invalid dataset)
are not retried automatically. The existing Step Functions retry policy covers
Lambda service errors separately; acquisition retries happen inside Python.

`CHAR_RENDER_SOURCE_FETCH_ATTEMPTS` accepts 1–3 (default 3), and
`CHAR_RENDER_SOURCE_FETCH_WORKERS` accepts 1–8 (default 6). Acquisition has a
240-second maximum network budget, additionally limited by Lambda's remaining
time minus ten seconds. Each HTTP request, including redirects, respects that
deadline. S3/SDK operations have their own SDK timeout behavior.

The real local TLS proxy regression verifies concurrent requests, inherited
cookies, and matching CONNECT proxy credentials. Other tests verify partial
success reuse, fresh-session recovery, bounded retries, and snapshot preservation.
These are correctness tests, not AWS throughput measurements.

### Job c7a6ce87-537c-4036-8a23-76638a8f7203

Read its Step Functions execution history: `FetchSources` failed in
`fetch_character_flashvars` → `fetch_text` → `fetch_bytes`, **before asset
planning/downloads**, with `SourceHttpError: AQW source connection failed; check
proxy, TLS and connectivity`. There was one failed Lambda invocation and no
acquisition retry. The old error hid the underlying curl code, so the history
cannot distinguish proxy connectivity, TLS, or timeout. New transport errors
include the numeric curl code without logging credentials. Retry logs contain
controlled error types/statuses; failed asset logs also identify the asset path.

No ARM image build, AWS deployment, or new render was performed for this change.

## Credentials

Only AWS needs Bright Data configuration now. Remove the old bot proxy environment
variable when upgrading the bot; CLI render submissions do not need it either.

Create `~/.config/aqw-render/brightdata.json` outside Git with mode 0600:

```json
{
  "server": "http://brd.superproxy.io:33335",
  "username": "brd-customer-YOUR_CUSTOMER-zone-YOUR_ZONE",
  "password": "YOUR_PROXY_PASSWORD"
}
```

Use the native proxy username/password, not a REST API token. Do not append
`-session-...` or `-const`; the fetch step adds them. Optional `ca_pem` adds the
provider certificate if your product requires it. The native proxy server must
be an HTTP URL with an explicit port and no embedded credentials or extra path.

Create the SecureString using the default AWS-managed SSM KMS key:

```bash
aws ssm put-parameter --profile aqw-char-dev --region us-west-2 \
  --name /aqw-char/dev/brightdata --type SecureString \
  --value "file://$HOME/.config/aqw-render/brightdata.json" --overwrite
```

The Lambda reads the SecureString at runtime. Only its parameter name appears in
CDK/Lambda configuration. A custom KMS key needs an additional decrypt grant.
The parameter defaults to `/aqw-char/dev/brightdata`; override its name through
`-c brightDataConfigParameter=/your/parameter` if needed.

## Deploy only the standalone fetch Lambda first

These are commands **for you to run**. This selects a separate CDK stack with
one Python fetch Lambda, its log group and IAM role, using the existing buckets.
It creates no render workflow, queues, database, or buckets and deploys no Rust
renderer image. It has no reserved concurrency allocation. CDK builds only the
Python ARM64 fetch image; you control that build/deployment.

From the infra repository, with AWS SSO active and Docker running:

```bash
SOURCE_BUCKET=$(aws cloudformation describe-stacks \
  --profile aqw-char-dev --region us-west-2 \
  --stack-name aqw-char-rendering-dev \
  --query "Stacks[0].Outputs[?OutputKey=='SourceAssetBucketName'].OutputValue | [0]" --output text)
WORK_BUCKET=$(aws cloudformation describe-stacks \
  --profile aqw-char-dev --region us-west-2 \
  --stack-name aqw-char-rendering-dev \
  --query "Stacks[0].Outputs[?OutputKey=='WorkResultBucketName'].OutputValue | [0]" --output text)

npx cdk deploy AqwSourceFetchTest --profile aqw-char-dev \
  -c environment=dev -c fetchOnly=true \
  -c sourceBucketName="$SOURCE_BUCKET" -c workBucketName="$WORK_BUCKET"
```

Generate a unique test payload and invoke the standalone Lambda:

```bash
python3 scripts/make_source_fetch_request.py 'queen iona' --output /tmp/queen-fetch.json
aws lambda invoke --profile aqw-char-dev --region us-west-2 \
  --function-name aqw-char-dev-source-fetch-test \
  --cli-binary-format raw-in-base64-out \
  --payload file:///tmp/queen-fetch.json --cli-read-timeout 360 \
  /tmp/queen-fetch-result.json
```

The invoke must have no `FunctionError`. HTTP `StatusCode: 200` by itself does
not prove the Lambda succeeded. The response file contains the enriched request;
S3 contains `jobs/<job_id>/fetch/request.json`, `appearance.json`, and
`sources.json`. The generator prints the relevant keys. Read `sources.json` to
see each source key and whether it was a cache hit:

```bash
JOB_ID=$(python3 -c 'import json; print(json.load(open("/tmp/queen-fetch.json"))["job_id"])')
aws s3 cp "s3://$WORK_BUCKET/jobs/$JOB_ID/fetch/sources.json" - \
  --profile aqw-char-dev --region us-west-2
```

Run the same invoke again to check completed-step reuse. Generate a new payload
and invoke it to test a fresh character fetch with cached SWFs. Also test
`yoshino` for the other animated background and a bank-pet character of your
choice. A cache hit will not exercise that SWF's origin download; do not delete
shared cached assets just to force a miss.

These tests make paid proxy requests and can add reusable SWFs to the existing
source bucket. They do not render images, reserve user slots, or notify Discord.
Only the completed acquisition response indicates success.

## Integrate after the standalone test succeeds

Deploy the normal infrastructure stack using your usual build/deploy workflow,
without `fetchOnly=true`, then update the bot. **Deploy AWS first:** the new bot
expects `FetchSources` and retry reference support. The updated AWS workflow can
still consume old bot requests that already contain appearance information.

Source acquisition is in `services/renderer/src/aqw_char_renderer/fetch_sources.py`;
HTTP transport in `source_http.py`; its lightweight image is
`services/renderer/Dockerfile.fetch`; CDK wiring is in `lib/source-fetch.ts` and the
main stack. The background source catalog is packaged with Python and tested
against the Rust catalog to prevent drift.

Local tests cover cookie continuity through a real local HTTPS CONNECT proxy,
TLS verification, session auth, bounded binary reads, planning/caching/retries,
no bot/CLI source calls, workflow order and failure routing, and standalone stack
isolation. Live proxy behavior and AWS runtime remain for your standalone test;
Mac tests do not establish AWS performance.
