"""Invoke one deployed component-compose Lambda with a full saved frame range."""

from __future__ import annotations

import argparse
import base64
import json
from pathlib import Path
from time import perf_counter
from typing import Any

import boto3
from aqw_char_renderer.stages.component_raster import component_workflow_result
from botocore.config import Config as BotoConfig


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser(description=__doc__)
    result.add_argument("--artifact-dir", type=Path, required=True)
    result.add_argument("--profile", default="aqw-char-dev")
    result.add_argument("--region", default="us-west-2")
    result.add_argument("--function-name", default="aqw-char-dev-componentcompose")
    result.add_argument("--runs", type=int, default=2)
    result.add_argument("--batch-index", type=int, default=9000)
    result.add_argument("--compositor", choices=("pillow",), default="pillow")
    result.add_argument("--frame-start", type=int, default=1)
    result.add_argument("--frame-end", type=int)
    result.add_argument(
        "--benchmark-output-prefix",
        type=str,
        help=(
            "S3 prefix (e.g. benchmarks/rust-compose/<job-id>) where frames and "
            "the compose-batch manifest are written instead of the job's normal "
            "jobs/<job-id>/component/... keys. Required for candidate functions "
            "so they never touch a completed job's intermediate frames."
        ),
    )
    return result


def invoke(client: Any, function_name: str, payload: dict[str, Any]) -> dict[str, Any]:
    started = perf_counter()
    response = client.invoke(
        FunctionName=function_name,
        InvocationType="RequestResponse",
        LogType="Tail",
        Payload=json.dumps(payload, separators=(",", ":")).encode(),
    )
    wall_seconds = perf_counter() - started
    body = json.loads(response["Payload"].read())
    log_tail = base64.b64decode(response.get("LogResult", "")).decode(
        "utf-8", errors="replace"
    )
    if response.get("FunctionError"):
        raise RuntimeError(
            f"Lambda failed: {json.dumps(body, default=str)}\n{log_tail}"
        )
    structured: list[dict[str, Any]] = []
    for line in log_tail.splitlines():
        start = line.find("{")
        if start < 0:
            continue
        try:
            record = json.loads(line[start:])
        except json.JSONDecodeError:
            continue
        if isinstance(record, dict):
            structured.append(record)
    return {
        "wall_seconds": wall_seconds,
        "response": body,
        "events": structured,
        "log_tail": log_tail,
    }


def main() -> int:
    args = parser().parse_args()
    if args.runs < 1:
        raise SystemExit("--runs must be positive")
    manifest = json.loads(
        (args.artifact_dir / "manifest.json").read_text(encoding="utf-8")
    )
    frame_count = int(manifest["frame_count"])
    frame_end = args.frame_end if args.frame_end is not None else frame_count
    if args.frame_start < 1 or frame_end < args.frame_start or frame_end > frame_count:
        raise SystemExit(
            f"frame range must be within 1-{frame_count}, got "
            f"{args.frame_start}-{frame_end}"
        )
    records = [
        json.loads(path.read_text(encoding="utf-8"))
        for path in sorted((args.artifact_dir / "component" / "results").glob("*.json"))
    ]
    event = {
        "job_id": str(manifest["job_id"]),
        "manifest_key": f"jobs/{manifest['job_id']}/prepare/manifest.json",
        "component_results": [component_workflow_result(record) for record in records],
        "compositor": args.compositor,
        "batch": {
            "index": args.batch_index,
            "frame_start": args.frame_start,
            "frame_end": frame_end,
        },
    }
    if args.benchmark_output_prefix:
        event["benchmark_output_prefix"] = args.benchmark_output_prefix
    client = boto3.Session(
        profile_name=args.profile,
        region_name=args.region,
    ).client(
        "lambda",
        config=BotoConfig(read_timeout=360, connect_timeout=10),
    )
    outputs = []
    for run_number in range(1, args.runs + 1):
        result = invoke(client, args.function_name, event)
        profile = next(
            (
                record
                for record in result["events"]
                if record.get("event") == "component_compose_profile"
            ),
            None,
        )
        completion = next(
            (
                record
                for record in result["events"]
                if record.get("event") == "component_compose_complete"
            ),
            None,
        )
        outputs.append(
            {
                "run": run_number,
                "wall_seconds": result["wall_seconds"],
                "profile": profile,
                "completion": completion,
                "response": result["response"],
                "log_tail": result["log_tail"] if profile is None else None,
            }
        )
        print(json.dumps(outputs[-1], indent=2, sort_keys=True, default=str))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
