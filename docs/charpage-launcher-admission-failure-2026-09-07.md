# Charpage job missing from Step Functions

Job: `42d40a0f-1a32-4821-817c-d935866767ff` (Fleki).

AWS inspection found the DynamoDB job still `QUEUED`, created at
2026-09-07 17:11:23 UTC, with `slot_released=false` and no execution ARN.
Step Functions returned `ExecutionDoesNotExist`. Launcher invocations repeated
at approximately three-minute intervals; the sampled X-Ray trace reports an
error, but CloudWatch did not include its error message.

The deployed launcher image is
`sha256:d81d402c85affccbf56070fbcf450d61b9a106c072f8a6cf920ca9f00bf8b861`.
An isolated local run of that existing image, with networking disabled, accepted
the saved charpage request and reached source-manifest lookup. Therefore this
is not simply a deployed image that rejects charpage settings.

## Reproduced admission mismatch and fix

The bot emits default viewport coordinates `[0.0,0.0,550.0,350.0]` in the queue
message. The actual DynamoDB job record stores `[0,0,550,350]`. DynamoDB numbers
do not preserve the integer-versus-floating JSON representation.

The launcher normalizes and compares the queued request to the admitted request
before starting Step Functions. Presentation normalization previously preserved
the two representations, making serde_json equality fail even though they have
the same coordinate values. This reproduces the admission-check failure locally;
the AWS trace itself does not expose the exact exception.

`presentation::normalize` now canonicalizes validated coordinates as floating
numbers before comparison and cache hashing. It still distinguishes genuinely
different layouts. The regression fails before this correction and passes after,
covering both viewport and character-position coordinates.

No AWS jobs were changed, submitted, or retried. No ARM build or deployment was
performed. The launcher needs the rebuilt pipeline image. A message already sent
to the dead-letter queue will not automatically resume on deployment; retry the
job after deployment. The existing cleanup routine eventually marks jobs that
remain queued without an execution for over an hour as failed and releases their
slots.

Evidence: `/tmp/aqw-charpage-missing-job.json`,
`/tmp/aqw-charpage-launcher-recent.json`, `/tmp/aqw-charpage-trace.json`.
