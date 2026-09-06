#!/usr/bin/env bash
# Operator-run after deployment. See docs/iir-blur-traversal-2026-09-06.md.
# Submits eight normal CLI renders; results stay on the command line, not Discord.
set -euo pipefail

IIR_REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$IIR_REPO_ROOT"
IIR_RESULTS_DIR="$(mktemp -d /tmp/aqw-iir-render-checks-XXXXXX)"
printf 'Saving render job IDs and results in %s\n' "$IIR_RESULTS_DIR"

for IIR_ROUND in 1 2 3; do
  scripts/render-character --restart 1aa28d78-b90a-4712-9f2f-59d4a34dda8e \
    --no-render-cache --no-component-cache --no-bounds-cache \
    --timeout 1800 \
    | tee "$IIR_RESULTS_DIR/annie-$IIR_ROUND.log"

  scripts/render-character --restart 5a400e21-8def-4fcc-92a4-675653d6ad20 \
    --no-render-cache --no-component-cache --no-bounds-cache \
    --timeout 1800 \
    | tee "$IIR_RESULTS_DIR/dalvi-$IIR_ROUND.log"
done

scripts/render-character --restart a28796e8-04d3-4c10-bb6e-9d72ca535145 \
  --no-render-cache --no-component-cache --no-bounds-cache \
  --timeout 1800 \
  | tee "$IIR_RESULTS_DIR/alina.log"

scripts/render-character --restart 6c7c0336-94a3-497f-a221-aa72d80851b5 \
  --no-render-cache --no-component-cache --no-bounds-cache \
  --timeout 1800 \
  | tee "$IIR_RESULTS_DIR/akine.log"

aws lambda get-function-configuration \
  --profile "${AWS_PROFILE:-aqw-char-dev}" \
  --region "${AWS_DEFAULT_REGION:-us-west-2}" \
  --function-name aqw-char-dev-componentraster-rust \
  --query '{CodeSha256:CodeSha256,RevisionId:RevisionId,LastModified:LastModified,MemorySize:MemorySize,Architectures:Architectures}' \
  --output json --no-cli-pager \
  > "$IIR_RESULTS_DIR/raster-worker.json"

printf 'Finished. Results: %s\n' "$IIR_RESULTS_DIR"
