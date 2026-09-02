"""Report per-stage timings for a completed AQW character render job.

Reads the Step Functions execution history (state wall-clock) and the Lambda
log groups (per-stage durations + per-phase breakdowns) and prints a summary.

Usage:
    AWS_PROFILE=aqw-char-dev AWS_DEFAULT_REGION=us-west-2 \
        .venv/bin/python scripts/render_timing_report.py <job_id>
"""

from __future__ import annotations

import argparse
import json
import sys
import time
from datetime import datetime
from typing import Any

import boto3

REGION = "us-west-2"
ACCOUNT = "538522204887"
STATE_MACHINE = "aqw-char-render-dev"

# Log groups by function name prefix
LOG_GROUPS = {
    "launcher": "/aws/lambda/aqw-char-dev-launcher",
    "prepare": "/aws/lambda/aqw-char-dev-prepare",
    "render": "/aws/lambda/aqw-char-dev-render",
    "finalizer": "/aws/lambda/aqw-char-dev-finalizer",
}

# Lambda log events we instrumented, in rough workflow order
PROFILE_EVENTS = (
    "prepare_resolve_profile",
    "prepare_export_complete",
    "prepare_profile",
    "render_batch_profile",
    "finalize_complete",
)

def _parse(ts: str | datetime) -> float:
    if isinstance(ts, datetime):
        return ts.timestamp()
    return datetime.fromisoformat(ts).timestamp()


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("job_id", help="Render job UUID (the Step Functions execution name)")
    args = parser.parse_args()
    job_id = args.job_id.strip()
    if not job_id:
        parser.error("job_id is required")

    sfn = boto3.client("stepfunctions", region_name=REGION)
    logs = boto3.client("logs", region_name=REGION)

    execution_arn = (
        f"arn:aws:states:{REGION}:{ACCOUNT}:execution:{STATE_MACHINE}:{job_id}"
    )
    print(f"Job: {job_id}\n")

    # --- Step Functions state-level timings ---
    history = sfn.get_paginator("get_execution_history")
    events: list[dict[str, Any]] = []
    for page in history.paginate(executionArn=execution_arn):
        events.extend(page.get("events", []))

    entered: dict[str, float] = {}
    state_durations: dict[str, float] = {}
    for event in events:
        etype = event.get("type")
        ts = _parse(event["timestamp"])
        if etype == "TaskStateEntered":
            name = event.get("stateEnteredEventDetails", {}).get("name")
            if name:
                entered[name] = ts
        elif etype in ("TaskStateExited", "MapStateExited"):
            name = event.get("stateExitedEventDetails", {}).get("name")
            if name and name in entered:
                state_durations[name] = ts - entered[name]

    if state_durations:
        print("== Step Functions state wall time ==")
        for name, duration in state_durations.items():
            print(f"  {name:28s} {duration:8.1f}s")
        print()

    # --- Lambda per-stage durations from the log groups ---
    print("== Lambda per-stage durations ==")
    for stage, log_group in LOG_GROUPS.items():
        messages: list[str] = []
        paginator = logs.get_paginator("filter_log_events")
        start_time = int((time.time() - 7 * 86400) * 1000)
        for page in paginator.paginate(
            logGroupName=log_group,
            filterPattern=f'"{job_id}"',
            startTime=start_time,
        ):
            for event in page.get("events", []):
                messages.append(event.get("message", ""))
        profiles = []
        for message in messages:
            try:
                data = json.loads(message)
            except (json.JSONDecodeError, TypeError):
                continue
            if data.get("event") in PROFILE_EVENTS and data.get("job_id") == job_id:
                profiles.append(data)
        if profiles:
            for data in profiles:
                parts = [f"{data.get('event')}"]
                for key in ("duration_ms", "total_ms"):
                    if key in data:
                        parts.append(f"{key}={data[key]}ms")
                print(f"  [{stage}] " + " ".join(parts))
        else:
            print(f"  [{stage}] (no profile events)")

    print()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())