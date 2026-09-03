"""Benchmark one deployed component-compose Lambda over a saved frame batch.

Reads the prepare manifest and compact component result records for a
completed job from S3, builds a real `ComposeComponentFrameChunk` event (the
same event the Step Functions map emits), and invokes the deployed function
directly. `--benchmark-output-prefix` keeps composed frames and the batch
manifest under a scratch prefix so a benchmark never touches a completed
job's intermediate keys.

Usage:
    uv run --package aqw-char-renderer python scripts/benchmark_compose_lambda.py \\
        --job-id 8396596b-0c17-4572-aaf9-21d87c732282 \\
        --function-name aqw-char-dev-componentcompose-rust \\
        --benchmark-output-prefix benchmarks/alina-simd \\
        [--runs 3] [--batch-index 9000]
"""
from __future__ import annotations

import argparse
import base64
import json
import sys
from pathlib import Path
from time import perf_counter
from typing import Any

import boto3

from aqw_char_renderer.stages.component_raster import component_workflow_result

WORK_BUCKET = "aqw-char-rendering-dev-workresultbucket6323ca9d-ango9x59ui8e"


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser(description=__doc__)
    result.add_argument("--job-id", required=True)
    result.add_argument("--bucket", default=WORK_BUCKET)
    result.add_argument("--profile", default="aqw-char-dev")
    result.add_argument("--region", default="us-west-2")
    result.add_argument(
        "--function-name",
        default="aqw-char-dev-componentcompose-rust",
        help="Deployed compose function to benchmark",
    )
    result.add_argument("--runs", type=int, default=3)
    result.add_argument("--batch-index", type=int, default=9000)
    result.add_argument("--frame-range", default="1-10")
    result.add_argument(
        "--benchmark-output-prefix",
        default="benchmarks/compose-lambda",
        help="S3 prefix for benchmark frames + manifest (never the job's keys)",
    )
    return result


def load_s3_json(s3: Any, bucket: str, key: str) -> dict[str, Any]:
    body = s3.get_object(Bucket=bucket, Key=key)["Body"].read()
    return json.loads(body)


def main() -> int:
    args = parser().parse_args()
    start_s, end_s = args.frame_range.split("-")
    frame_start, frame_end = int(start_s), int(end_s)

    session = boto3.Session(profile_name=args.profile, region_name=args.region)
    s3 = session.client("s3")
    manifest = load_s3_json(s3, args.bucket, f"jobs/{args.job_id}/prepare/manifest.json")
    frame_count = int(manifest["frame_count"])
    if not 1 <= frame_start <= frame_end <= frame_count:
        raise SystemExit(f"frame range must be within 1-{frame_count}")

    records = []
    paginator = s3.get_paginator("list_objects_v2")
    for page in paginator.paginate(
        Bucket=args.bucket, Prefix=f"jobs/{args.job_id}/component/results/"
    ):
        for obj in page.get("Contents", []):
            records.append(load_s3_json(s3, args.bucket, obj["Key"]))
    if not records:
        raise SystemExit(f"no component result records for {args.job_id}")

    event = {
        "job_id": str(manifest["job_id"]),
        "manifest_key": f"jobs/{manifest['job_id']}/prepare/manifest.json",
        "component_results": [component_workflow_result(r) for r in records],
        "batch": {"index": args.batch_index, "frame_start": frame_start, "frame_end": frame_end},
    }
    if args.benchmark_output_prefix:
        event["benchmark_output_prefix"] = args.benchmark_output_prefix

    client = session.client(
        "lambda", config=boto3.session.Config(read_timeout=360, connect_timeout=10)
    )
    username = (manifest.get("settings") or {}).get("username", "?")
    frame_pixels = int(manifest.get("viewbox", [0, 0, 1, 1])[2] or 0)
    summary = {
        "job_id": args.job_id,
        "username": username,
        "frame_count": frame_count,
        "frame_range": f"{frame_start}-{frame_end}",
        "frames_in_batch": frame_end - frame_start + 1,
        "function": args.function_name,
        "output_prefix": args.benchmark_output_prefix,
        "runs": args.runs,
        "runs_detail": [],
    }

    for run_number in range(1, args.runs + 1):
        started = perf_counter()
        response = client.invoke(
            FunctionName=args.function_name,
            InvocationType="RequestResponse",
            LogType="Tail",
            Payload=json.dumps(event, separators=(",", ":")).encode(),
        )
        wall_seconds = perf_counter() - started
        body = json.loads(response["Payload"].read())
        log_tail = base64.b64decode(response.get("LogResult", "")).decode(
            "utf-8", errors="replace"
        )
        if response.get("FunctionError"):
            raise RuntimeError(f"Lambda failed: {json.dumps(body, default=str)}\n{log_tail}")
        profile = None
        for line in log_tail.splitlines():
            start = line.find("{")
            if start < 0:
                continue
            try:
                record = json.loads(line[start:])
            except json.JSONDecodeError:
                continue
            if isinstance(record, dict) and record.get("event") == "component_compose_profile":
                profile = record
        run_detail = {
            "run": run_number,
            "wall_seconds": round(wall_seconds, 4),
            "profile": profile,
        }
        summary["runs_detail"].append(run_detail)
        print(json.dumps(run_detail, indent=2, sort_keys=True, default=str))
        sys.stdout.flush()

    timings = {f"run{n}_wall": r["wall_seconds"] for n, r in enumerate(summary["runs_detail"], 1)}
    summary.update(timings)
    print(json.dumps({"summary": summary}, indent=2, sort_keys=True, default=str))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())