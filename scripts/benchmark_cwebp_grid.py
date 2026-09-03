"""Run a grid of cwebp encode configurations through the real pipeline and
collect size / timing / URL per configuration.

For each (quality, method, lossless) combo:
  1. Queue a full render of `username` through the SQS workflow via
     scripts/smoke_test_deployment.py with the cwebp flags exposed there.
  2. Stream the smoke output, stamping each line so wall-clock "queued ->
     SUCCEEDED" time is exact.
  3. Query CloudWatch Logs for the compose worker to sum per-batch
     `composite_ms` / `encode_ms` for that job (the ComposeComponentFrameChunk
     stage runs once per 10-frame batch).
  4. Emit one table row: q | m | lossless | final bytes | full render sec |
     compose sec | encode sec | cloudfront URL.

Usage:
    uv run --package aqw-char-renderer python scripts/benchmark_cwebp_grid.py \
        --username alina --max-frames 120 --output-size 2048
"""
from __future__ import annotations

import argparse
import json
import subprocess
import sys
import time
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
    result.add_argument("--username", default="alina")
    result.add_argument("--max-frames", type=int, default=120)
    result.add_argument("--output-size", type=int, choices=(256, 512, 1024, 2048), default=2048)
    result.add_argument("--profile", default="aqw-char-dev")
    result.add_argument("--region", default="us-west-2")
    result.add_argument(
        "--log-group",
        default="/aws/lambda/aqw-char-dev-componentcompose-rust",
    )
    result.add_argument("--only", help="comma list of combo indexes (0-based) to run")
    return result


def _stream_json(process: subprocess.Popen[str]) -> tuple[dict[str, Any], dict[str, float]]:
    """Stream subprocess stdout, stamping lines; yield complete JSON objects even
    when pretty-printed across multiple lines (the smoke 'verified' summary)."""
    timestamps: dict[str, float] = {}
    summary: dict[str, Any] | None = None
    decoder = json.JSONDecoder()
    buffer = ""
    assert process.stdout is not None
    for line in process.stdout:
        line = line.rstrip("\n")
        now = time.time()
        print(f"[{now:.1f}] {line}", flush=True)
        buffer += line
        cursor = 0
        while True:
            while cursor < len(buffer) and buffer[cursor] in " \t\r\n":
                cursor += 1
            if cursor >= len(buffer) or buffer[cursor] != "{":
                break
            try:
                record, end = decoder.raw_decode(buffer, cursor)
            except json.JSONDecodeError:
                break
            cursor = end
            if record.get("event") == "queued":
                timestamps["queued"] = now
            elif record.get("event") == "status" and record.get("status") == "SUCCEEDED":
                timestamps["succeeded"] = now
            elif record.get("event") == "verified":
                summary = record
        buffer = buffer[cursor:]
    process.wait()
    if process.returncode != 0:
        raise RuntimeError(f"smoke render failed: {process.returncode}")
    if summary is None:
        raise RuntimeError("no verified summary in smoke output")
    return summary, timestamps


def run_smoke(
    username: str,
    max_frames: int,
    output_size: int,
    quality: float,
    method: int,
    lossless: bool | None,
    *,
    profile: str,
    region: str,
) -> tuple[dict[str, Any], dict[str, float]]:
    """Run one smoke render; return (summary json, {queued_ts, succeeded_ts})."""
    script = Path(__file__).parent / "smoke_test_deployment.py"
    command = [
        sys.executable,
        str(script),
        "--username",
        username,
        "--max-frames",
        str(max_frames),
        "--output-size",
        str(output_size),
        "--webp-quality",
        str(quality),
        "--webp-method",
        str(method),
        "--timeout-seconds",
        "2400",
        "--poll-seconds",
        "5",
    ]
    if lossless:
        command.append("--webp-lossless")
    env = {
        **__import__("os").environ,
        "AWS_PROFILE": profile,
        "AWS_DEFAULT_REGION": region,
    }
    process = subprocess.Popen(
        command,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        env=env,
        bufsize=1,
    )
    summary, timestamps = _stream_json(process)
    return summary, timestamps


def fetch_compose_timings(
    job_id: str,
    start_ts: float,
    end_ts: float,
    *,
    profile: str,
    region: str,
    log_group: str,
) -> dict[str, float]:
    client = boto3.Session(profile_name=profile, region_name=region).client("logs")
    query = (
        f'filter event = "component_compose_profile" and job_id = "{job_id}" '
        "| stats sum(encode_ms) as encode, sum(composite_ms) as composite, "
        "sum(total_ms) as total, count(*) as batches"
    )
    started = client.start_query(
        logGroupName=log_group,
        startTime=int(start_ts) - 60,
        endTime=int(end_ts) + 60,
        queryString=query,
    )
    query_id = started["queryId"]
    for _ in range(30):
        time.sleep(2)
        result = client.get_query_results(queryId=query_id)
        if result["status"] == "Complete":
            rows = result["results"]
            if not rows:
                return {"encode_ms": 0.0, "composite_ms": 0.0, "total_ms": 0.0, "batches": 0}
            pairs = [(f["field"], float(f["value"])) for f in rows[0] if isinstance(f, dict)]
            return dict(pairs)
    raise TimeoutError(f"CloudWatch query {query_id} did not complete")


def main() -> int:
    args = parser().parse_args()
    only = {int(i) for i in args.only.split(",")} if args.only else None
    rows: list[dict[str, Any]] = []
    for index, (quality, method, lossless) in enumerate(COMBOS):
        if only is not None and index not in only:
            continue
        label = f"{quality:g} | {method} | {'lossless' if lossless else 'null'}"
        print(f"\n=== combo {index}: {label} ===", flush=True)
        try:
            started = time.time()
            summary, timestamps = run_smoke(
                args.username,
                args.max_frames,
                args.output_size,
                quality,
                method,
                lossless,
                profile=args.profile,
                region=args.region,
            )
        except Exception as error:  # keep the grid alive if one combo fails
            print(f"combo {index} ({label}) FAILED: {error}", flush=True)
            rows.append(
                {
                    "q": quality,
                    "m": method,
                    "lossless": bool(lossless),
                    "error": str(error),
                }
            )
            continue
        job_id = str(summary["job_id"])
        queued = timestamps.get("queued", started)
        succeeded = timestamps.get("succeeded", time.time())
        full_seconds = succeeded - queued
        timings = fetch_compose_timings(
            job_id,
            queued,
            time.time(),
            profile=args.profile,
            region=args.region,
            log_group=args.log_group,
        )
        row = {
            "q": quality,
            "m": method,
            "lossless": bool(lossless),
            "job_id": job_id,
            "bytes": int(summary.get("content_length") or summary.get("bytes") or 0),
            "full_render_seconds": round(full_seconds, 1),
            "compose_seconds": round(timings.get("composite_ms", 0.0) / 1000.0, 2),
            "encode_seconds": round(timings.get("encode_ms", 0.0) / 1000.0, 2),
            "compose_batches": int(timings.get("batches", 0)),
            "url": summary.get("url", ""),
        }
        rows.append(row)
        print(json.dumps(row, indent=2, default=str), flush=True)

    print("\n=== TABLE ===")
    header = ["q", "m", "lossless", "bytes", "full_s", "compose_s", "encode_s", "batches", "url"]
    print("| " + " | ".join(header) + " |")
    print("|" + "|".join(["---"] * len(header)) + "|")
    for row in rows:
        cells = [
            str(row["q"]),
            str(row["m"]),
            "lossless" if row["lossless"] else "null",
            str(row["bytes"]),
            str(row["full_render_seconds"]),
            str(row["compose_seconds"]),
            str(row["encode_seconds"]),
            str(row["compose_batches"]),
            row["url"],
        ]
        print("| " + " | ".join(cells) + " |")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())