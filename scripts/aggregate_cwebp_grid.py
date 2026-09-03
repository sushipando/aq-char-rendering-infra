"""Post-process a benchmark_cwebp_grid run into the final table.

Reads the grid log for job ids, pulls created_at/updated_at from the job
table (full render time) and per-stage encode/composite totals from
CloudWatch (the compose worker runs once per 10-frame batch), then prints a
markdown table.

Usage:
    uv run --package aqw-char-renderer python scripts/aggregate_cwebp_grid.py \
        --grid-log /tmp/cwebp_grid.log
"""
from __future__ import annotations

import argparse
import json
import re
import time
from datetime import datetime, timedelta
from pathlib import Path
from typing import Any

import boto3

COMBOS: list[tuple[float, int, bool | None]] = [
    (75, 4, None),
    (82, 4, None),
    (85, 4, None),
    (70, 1, None),
    (70, 2, None),
    (75, 4, True),
    (0, 0, True),
    (20, 1, True),
    (25, 2, True),
    (30, 3, True),
    (50, 3, True),
    (60, 4, True),
]


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser(description=__doc__)
    result.add_argument("--grid-log", type=Path, default=Path("/tmp/cwebp_grid.log"))
    result.add_argument("--job-table", default="aqw-char-render-jobs-dev")
    result.add_argument("--profile", default="aqw-char-dev")
    result.add_argument("--region", default="us-west-2")
    result.add_argument(
        "--log-group",
        default="/aws/lambda/aqw-char-dev-componentcompose-rust",
    )
    return result


def parse_log(path: Path) -> list[dict[str, Any]]:
    rows = [None] * 12
    current: dict[str, Any] | None = None
    for line in path.read_text().splitlines():
        match = re.search(r"=== combo (\d+):", line)
        if match:
            current = {"index": int(match.group(1))}
            continue
        if current is None:
            continue
        match = re.search(r'"(job_id|bytes|url|q|m|lossless)":\s*"?([^",}]+)"?', line)
        if match and match.group(1) == "job_id":
            current["job_id"] = match.group(2)
        if match and match.group(1) == "url":
            current["url"] = match.group(2)
        if "content_length" in line and ":" in line:
            digit = re.search(r'"content_length":\s*(\d+)', line)
            if digit:
                current["bytes"] = int(digit.group(1))
        if line.strip() == "}":
            if "job_id" in current:
                rows[current["index"]] = current
            current = None
    return rows


def fetch_compose_totals(
    logs: Any, job_id: str, start_ts: float, end_ts: float, log_group: str
) -> dict[str, float]:
    query = (
        f'filter event = "component_compose_profile" and job_id = "{job_id}" '
        "| stats sum(encode_ms) as encode, sum(composite_ms) as composite, "
        "sum(total_ms) as total, count(*) as batches"
    )
    started = logs.start_query(
        logGroupName=log_group,
        startTime=int(start_ts) - 300,
        endTime=int(end_ts) + 300,
        queryString=query,
    )
    query_id = started["queryId"]
    for _ in range(30):
        time.sleep(2)
        result = logs.get_query_results(queryId=query_id)
        if result["status"] == "Complete":
            rows = result["results"]
            if not rows:
                return {}
            return {f["field"]: float(f["value"]) for f in rows[0] if isinstance(f, dict)}
    raise TimeoutError(f"query {query_id} incomplete")


def fetch_compose_stage_wall(
    logs: Any, job_id: str, start_ts: float, end_ts: float, log_group: str
) -> tuple[float, float]:
    """Return (stage wall seconds, batch count) from the per-batch complete
    events: the ComposeComponentFrameChunk map runs up to concurrency batches
    at once, so stage wall = latest batch end - earliest batch start."""
    query = (
        f'fields @timestamp, duration_ms | filter event = "component_compose_complete" '
        f'and job_id = "{job_id}" | sort @timestamp asc | limit 500'
    )
    started = logs.start_query(
        logGroupName=log_group,
        startTime=int(start_ts) - 300,
        endTime=int(end_ts) + 300,
        queryString=query,
    )
    query_id = started["queryId"]
    for _ in range(30):
        time.sleep(2)
        result = logs.get_query_results(queryId=query_id)
        if result["status"] == "Complete":
            rows = result["results"]
            if not rows:
                return 0.0, 0.0
            stamps = []
            for row in rows:
                fields = {f["field"]: f["value"] for f in row if isinstance(f, dict)}
                ts = datetime.fromisoformat(fields["@timestamp"].replace("Z", "+00:00"))
                stamps.append((ts, float(fields.get("duration_ms", 0))))
            start = stamps[0][0]
            end = max(ts + timedelta(milliseconds=ms) for ts, ms in stamps)
            return (end - start).total_seconds(), len(stamps)
    raise TimeoutError(f"query {query_id} incomplete")


def main() -> int:
    args = parser().parse_args()
    session = boto3.Session(profile_name=args.profile, region_name=args.region)
    dy = session.client("dynamodb")
    logs = session.client("logs")

    rows = parse_log(args.grid_log)
    print(len([r for r in rows if r]), "combo rows in grid log")

    results: list[dict[str, Any]] = []
    for combo, (quality, method, lossless) in enumerate(COMBOS):
        row = rows[combo]
        label = f"{quality:g} | {method} | {'lossless' if lossless else 'null'}"
        if not row:
            results.append({"combo": combo, "label": label, "error": "no job in grid log"})
            continue
        job_id = row["job_id"]
        item = dy.get_item(
            TableName=args.job_table,
            Key={"PK": {"S": f"JOB#{job_id}"}, "SK": {"S": "META"}},
        )["Item"]
        created = item["created_at"]["S"]
        updated = item["updated_at"]["S"]
        created_dt = datetime.fromisoformat(created.replace("Z", "+00:00"))
        updated_dt = datetime.fromisoformat(updated.replace("Z", "+00:00"))
        full_seconds = (updated_dt - created_dt).total_seconds()

        totals = fetch_compose_totals(
            logs, job_id, created_dt.timestamp(), updated_dt.timestamp(), args.log_group
        )
        stage_seconds, stage_batches = fetch_compose_stage_wall(
            logs, job_id, created_dt.timestamp(), updated_dt.timestamp(), args.log_group
        )
        results.append(
            {
                "combo": combo,
                "q": quality,
                "m": method,
                "lossless": bool(lossless),
                "label": label,
                "bytes": row.get("bytes", 0),
                "full_render_seconds": round(full_seconds, 1),
                "compose_stage_seconds": round(stage_seconds, 1),
                "encode_seconds": round(totals.get("encode", 0.0) / 1000.0, 2),
                "batches": stage_batches or int(totals.get("batches", 0)),
                "url": row.get("url", ""),
            }
        )

    print("\n| q | m | lossless | bytes | full_render_s | compose_stage_s | encode_s | url |")
    print("|---|---|---|---|---|---|---|---|---|")
    for r in results:
        if "error" in r:
            print(f"| {r['label']} | ERROR {r['error']} |")
            continue
        print(
            f"| {r['q']:g} | {r['m']} | {'lossless' if r['lossless'] else 'null'} | "
            f"{r['bytes']} | {r['full_render_seconds']} | {r['compose_stage_seconds']} | "
            f"{r['encode_seconds']} | {r['batches']} | {r['url']} |"
        )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())