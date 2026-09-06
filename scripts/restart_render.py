"""Restart the full normal workflow with a fresh job ID and the original inputs.

scripts/render-character --restart JOB_ID [--no-render-cache | --no-cache]
No redrive, no compose-only branch, and no copying old raster artifacts.
"""

from __future__ import annotations

import argparse
import copy
import json
from dataclasses import asdict, replace
from pathlib import Path
from uuid import UUID, uuid4

import boto3
from aqw_char_renderer.contracts import JobRequest, utc_now
from aqw_char_renderer.jobs import JobStore
from botocore.exceptions import ClientError
from smoke_test_deployment import load_outputs
from submit_render import enqueue_request, wait_for

CACHE_FLAGS = {
    "render": "render",
    "animation": "animation",
    "vectors": "vector",
    "bounds": "bounds",
    "components": "component",
}


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser(description=__doc__)
    result.add_argument(
        "source", help="Original job ID, execution name (UUID), or execution ARN"
    )
    result.add_argument("--outputs", type=Path, default=Path("cdk-outputs.dev.json"))
    result.add_argument(
        "--dry-run",
        action="store_true",
        help="Read and print the new request without admitting or submitting it",
    )
    result.add_argument("--no-watch", action="store_true")
    result.add_argument("--timeout", type=int, default=900)
    result.add_argument("--poll", type=float, default=2)
    result.add_argument("--max-active", type=int, default=2)
    result.add_argument(
        "--no-cache",
        action="store_true",
        help="Explicitly disable every cache; otherwise preserve the original cache settings",
    )
    for flag in CACHE_FLAGS.values():
        result.add_argument(
            f"--no-{flag}-cache",
            action="store_true",
            help=f"Disable only the {flag} cache",
        )
    return result


def load_original_request(
    outputs: dict[str, str], source: str, jobs, sfn, s3
) -> tuple[JobRequest, str]:
    prefix = outputs["StateMachineArn"].replace(":stateMachine:", ":execution:") + ":"
    if source.startswith("arn:"):
        arn = source
    else:
        source = str(UUID(source))
        record = jobs.get(source)
        arn = (record or {}).get("execution_arn") or prefix + source
    if not arn.startswith(prefix) or not arn[len(prefix) :]:
        raise ValueError(
            "Source execution must belong to the configured render state machine"
        )
    execution = sfn.describe_execution(executionArn=arn)
    # Use the hydrated execution input, not a sparse admission request. This
    # preserves the original defaults even when fleet defaults have changed.
    original = json.loads(execution["input"])["request"]
    request = JobRequest.from_dict(original)
    if request.appearance is None:
        # Some callers originally let PrepareResolve fetch the appearance.
        # Recover its saved snapshot, never today's equipment/CC from AQW.
        bucket = outputs["WorkResultBucketName"]
        for filename in ("input.json", "manifest.json"):
            try:
                saved = json.loads(
                    s3.get_object(
                        Bucket=bucket, Key=f"jobs/{request.job_id}/prepare/{filename}"
                    )["Body"].read()
                )
            except ClientError as error:
                if error.response["Error"]["Code"] in {"NoSuchKey", "404", "NotFound"}:
                    continue
                raise
            if (
                saved.get("job_id") != request.job_id
                or saved.get("settings") != request.render.to_dict()
            ):
                raise ValueError(
                    "Saved appearance snapshot does not match the original job/settings"
                )
            if not saved.get("fields"):
                continue
            original["appearance"] = saved["fields"]
            request = JobRequest.from_dict(original)
            break
        if request.appearance is None:
            raise RuntimeError(
                "Original appearance was not in the request and its saved snapshot is unavailable. Cannot restart with identical assets; submit a username to render today's appearance instead."
            )
    return request, arn


def fresh_request(original: JobRequest, args: argparse.Namespace) -> JobRequest:
    cache = asdict(original.cache)
    for field, flag in CACHE_FLAGS.items():
        if args.no_cache or getattr(args, f"no_{flag}_cache"):
            cache[field] = False
    return replace(
        copy.deepcopy(original),
        job_id=str(uuid4()),
        created_at=utc_now(),
        cache=replace(original.cache, **cache),
    )


def main() -> int:
    args = parser().parse_args()
    if args.max_active < 1 or args.timeout < 1 or args.poll <= 0:
        raise SystemExit("--max-active, --timeout and --poll must be positive")
    outputs = load_outputs(args.outputs.resolve())
    original, arn = load_original_request(
        outputs,
        args.source,
        JobStore(outputs["JobTableName"]),
        boto3.client("stepfunctions"),
        boto3.client("s3"),
    )
    request = fresh_request(original, args)
    summary = {
        "job_id": request.job_id,
        "restarted_from_job_id": original.job_id,
        "source_execution_arn": arn,
    }
    if args.dry_run:
        print(
            json.dumps(
                {**summary, "dry_run": True, "request": request.to_dict()}, indent=2
            )
        )
        print("Nothing admitted, submitted, or modified.")
        return 0
    # The SAME validation/admission/queue path as a fresh username submission.
    # Launcher creates an execution of the currently deployed state machine;
    # its input contains only request, with no alternate entry point.
    job, _ = enqueue_request(outputs, request, args.max_active)
    print(json.dumps({**summary, "event": "queued"}), flush=True)
    if args.no_watch:
        return 0
    record = wait_for(
        outputs, job, timeout_seconds=args.timeout, poll_seconds=args.poll
    )
    print(
        json.dumps(
            {
                "job_id": job,
                "status": record["status"],
                "url": record.get("result_url"),
            },
            default=str,
        )
    )
    return 0 if record["status"] in {"SUCCEEDED", "CACHE_HIT"} else 1


if __name__ == "__main__":
    raise SystemExit(main())
