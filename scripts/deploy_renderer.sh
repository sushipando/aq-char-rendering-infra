#!/usr/bin/env bash
# Deploy the AQW character renderer stack to the dev environment.
# Operator-run only: agents and unattended automation must stop after checks
# and diff review; the repository owner invokes this script for deployments.
#
# Handles the whole loop so neither the bot author nor CI needs to remember the
# SSO profile, region, account, or the exact CDK incantation:
#
#   1. auth       — validates the aqw-char-dev SSO session (logs in if missing)
#   2. docker     — confirms the daemon is up (Lambda images are built locally)
#   3. checks     — npm ci, tsc build, jest, cdk synth (read-only), cdk diff
#   4. bootstrap  — auto-detects and runs `cdk bootstrap` exactly when needed
#   5. deploy     — `cdk deploy --require-approval broadening` (auto-approve w/ --yes)
#   6. smoke      — optional: queue a real resvg render to close the loop
#
# Usage:
#   scripts/deploy_renderer.sh [--sso] [--yes] [--bootstrap] [--skip-checks]
#                             [--smoke [USERNAME]] [--dry-run] [--help]
#
# Flags:
#   --sso           force `aws sso login` (opens the browser) before validating
#   --yes           skip the final confirmation and use `--require-approval never`
#   --bootstrap     force `cdk bootstrap` even if the marker looks present
#   --skip-checks   skip npm ci/build/test/synth/diff (deploy immediately)
#   --smoke [USER]  after deploy, queue USER through scripts/render-character
#                   and wait for the resvg WebP (default: alina)
#   --dry-run       run auth/docker/checks/bootstrap-detection, then stop
#                   (never deploys; safe to run anywhere)

set -euo pipefail

readonly REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

# ---- target environment (single source of truth, mirrors README) -------------
readonly PROFILE="aqw-char-dev"
readonly REGION="us-west-2"
readonly ACCOUNT="538522204887"
# Marker `cdk bootstrap` writes (SSM param referenced by the synth template).
readonly BOOTSTRAP_PARAM="/cdk-bootstrap/hnb659fds/version"
SMOKE_USERNAME="${SMOKE_USERNAME:-alina}"

# ---- flags -------------------------------------------------------------------
FORCE_SSO=0
AUTO_YES=0
FORCE_BOOTSTRAP=0
SKIP_CHECKS=0
SMOKE=0
DRY_RUN=0

usage() {
  sed -n '2,27p' "$0" | sed 's/^# \{0,1\}//' >&2
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --sso) FORCE_SSO=1 ;;
    --yes) AUTO_YES=1 ;;
    --bootstrap) FORCE_BOOTSTRAP=1 ;;
    --skip-checks) SKIP_CHECKS=1 ;;
    --smoke)
      SMOKE=1
      if [[ $# -gt 1 && "$2" != -* ]]; then
        SMOKE_USERNAME="$2"
        shift
      fi
      ;;
    --smoke=*)
      SMOKE=1
      SMOKE_USERNAME="${1#*=}"
      if [[ -z "$SMOKE_USERNAME" ]]; then
        echo "--smoke requires a non-empty username after '='" >&2
        exit 2
      fi
      ;;
    --dry-run) DRY_RUN=1 ;;
    --help|-h) usage; exit 0 ;;
    *) echo "unknown argument: $1" >&2; usage; exit 2 ;;
  esac
  shift
done

# Injected into every child command; fixes the ambient-creds failure mode
# (boto3/CLI silently falling back to a personal account without a profile).
export AWS_PROFILE="$PROFILE"
export AWS_DEFAULT_REGION="$REGION"
export AWS_PAGER=""

# ---- 1. auth -----------------------------------------------------------------
sso_login() {
  echo ">> aws sso login --profile $PROFILE (browser will open)"
  aws sso login --profile "$PROFILE"
}

if (( FORCE_SSO )); then
  sso_login
fi

if ! identity="$(aws sts get-caller-identity --profile "$PROFILE" --region "$REGION" 2>&1)"; then
  echo "!! no valid $PROFILE session, logging in"
  sso_login
  identity="$(aws sts get-caller-identity --profile "$PROFILE" --region "$REGION")"
fi

if ! grep -q "\"Account\": \"$ACCOUNT\"" <<<"$identity"; then
  echo "!! identity is not the renderer account; expected $ACCOUNT, got:" >&2
  echo "$identity" >&2
  exit 1
fi
echo ">> auth ok: $(grep -o '"Arn": *"[^"]*"' <<<"$identity" | head -1)"

# ---- 2. docker ---------------------------------------------------------------
if ! docker info >/dev/null 2>&1; then
  echo "!! Docker daemon is not reachable; start Docker Desktop first" >&2
  exit 1
fi
echo ">> docker daemon ok"

# ---- 3. checks ---------------------------------------------------------------
if (( ! SKIP_CHECKS )); then
  echo ">> npm ci"
  npm ci --silent
  echo ">> npm run build"
  npm run build
  echo ">> npm test"
  npm test --silent
  echo ">> npm run synth -- --profile $PROFILE"
  npm run synth -- --profile "$PROFILE"
  echo ">> npm run diff -- --profile $PROFILE"
  npm run diff -- --profile "$PROFILE" || true
else
  echo ">> checks skipped (--skip-checks)"
fi

# ---- 4. bootstrap ------------------------------------------------------------
bootstrap() {
  echo ">> cdk bootstrap aws://$ACCOUNT/$REGION"
  npx cdk bootstrap "aws://$ACCOUNT/$REGION" --profile "$PROFILE"
}

if (( FORCE_BOOTSTRAP )); then
  bootstrap
elif aws ssm get-parameter --name "$BOOTSTRAP_PARAM" --profile "$PROFILE" --region "$REGION" >/dev/null 2>&1; then
  echo ">> bootstrap marker present ($BOOTSTRAP_PARAM); skipping"
else
  echo ">> bootstrap marker absent; bootstrapping"
  bootstrap
fi

if (( DRY_RUN )); then
  echo
  echo "DRY-RUN: all preflight steps passed; deploy/smoke skipped."
  echo "Ready to run: scripts/deploy_renderer.sh${AUTO_YES:+ --yes}"
  exit 0
fi

# ---- 5. deploy ---------------------------------------------------------------
echo
printf 'Target: aqw-char-render-dev  %s/%s  profile=%s\n\n' "$ACCOUNT" "$REGION" "$PROFILE"
if (( ! AUTO_YES )); then
  read -r -p "Deploy to dev ($ACCOUNT/$REGION)? This creates billable AWS resources and a ~15-30 min Docker image build. [y/N] " answer
  [[ "${answer,,}" =~ ^y(es)?$ ]] || { echo "aborted"; exit 1; }
fi

echo ">> npm run deploy -- --profile $PROFILE (long; container images build locally,"
echo "   incl. ARM64 Rust renderer images; warm Cargo caches speed up rebuilds)"
approval="--require-approval broadening"
(( AUTO_YES )) && approval="--require-approval never"
npm run deploy -- --profile "$PROFILE" $approval

# ---- 6. smoke -----------------------------------------------------------------
if (( SMOKE )); then
  echo ">> queueing uncached inline smoke render: $SMOKE_USERNAME (resvg, 1024)"
  AWS_PROFILE="$PROFILE" AWS_DEFAULT_REGION="$REGION" \
    scripts/render-character "$SMOKE_USERNAME" --output-size 1024 --bounds-mode inline \
      --component-raster-mode inline --no-cache
else
  echo
  echo "Deploy finished. Re-run with --smoke USERNAME to queue a resvg render, or submit one:"
  echo "  scripts/render-character alina"
fi
