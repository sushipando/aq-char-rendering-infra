"""Queue AQW character renders through the deployed workflow and wait for them.

A thin, ergonomic wrapper around the same SQS / DynamoDB admission path the
Discord bot uses: it fetches the character's equipped flashvars, seeds any
missing source assets, reserves a per-user slot, enqueues the render job, and
polls the job table until each job reaches a terminal state. It prints one
compact result row per character and (optionally) verifies the delivered WebP
on CloudFront.

Examples:
    # Single default render (artix, 2048, q85/m4 lossy)
    uv run --package aqw-char-renderer python scripts/submit_render.py

    # One character with the WebP toggles
    uv run --package aqw-char-renderer python scripts/submit_render.py alina \\
        --output-size 1024 --webp-quality 70 --webp-method 2

    # Multiple characters in one run (each gets its own job)
    uv run --package aqw-char-renderer python scripts/submit_render.py \\
        alina godlow --webp-lossless

    # Queue without waiting
    uv run --package aqw-char-renderer python scripts/submit_render.py alina --no-watch
"""

from __future__ import annotations

import argparse
import json
import time
from pathlib import Path
from typing import Any
from uuid import uuid4

import boto3
from aqw_char_renderer.contracts import (
    DiscordTarget,
    JobRequest,
    RenderSettings,
    utc_now,
)
from aqw_char_renderer.jobs import TERMINAL_STATUSES, JobStore
from aqw_char_renderer.legacy import preview_aqw_tryon as tryon

# Proven helpers from the smoke harness (same repo, importable module).
from smoke_test_deployment import load_outputs, seed_missing_assets, verify_webp


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser(
        description="Queue deployed AQW character renders (with WebP toggles) and watch them.",
    )
    result.add_argument("usernames", nargs="*", help="Public AQW character name(s)")
    result.add_argument("--outputs", type=Path, default=Path("cdk-outputs.dev.json"))
    result.add_argument("--output-size", type=int, choices=(256, 512, 1024, 2048), default=2048)
    result.add_argument("--webp-quality", type=float, default=85.0, help="cwebp -q 0..100 (default 85)")
    result.add_argument("--webp-method", type=int, default=4, help="cwebp -m 0..6 (default 4)")
    result.add_argument(
        "--raster-backend",
        choices=("resvg", "thorvg"),
        default="resvg",
        help="SVG rasterizer for the component pass: resvg (pinned 0.48.1, default) or thorvg (1.1.1)",
    )
    result.add_argument(
        "--webp-lossless",
        action="store_true",
        help="Encode frames with cwebp -lossless instead of lossy",
    )
    result.add_argument("--max-frames", type=int, default=120, help="animation frame count (default 120)")
    result.add_argument("--user-id", default="900000000000000001")
    result.add_argument("--channel-id", default="900000000000000002")
    result.add_argument("--guild-id", default="900000000000000003")
    result.add_argument("--dataset-version", default="dev-v1")
    result.add_argument("--max-active", type=int, default=2)
    result.add_argument("--timeout-seconds", type=int, default=1_800)
    result.add_argument("--poll-seconds", type=float, default=5)
    result.add_argument(
        "--no-verify",
        action="store_true",
        help="Skip the CloudFront WebP fetch/validation at the end",
    )
    result.add_argument(
        "--watch",
        dest="watch",
        action="store_true",
        default=True,
        help="Poll the job table until each job is terminal (default)",
    )
    result.add_argument(
        "--no-watch",
        dest="watch",
        action="store_false",
        help="Queue and return immediately (print job ids, no waiting)",
    )
    return result


def queue_one(
    outputs: dict[str, str],
    username: str,
    args: argparse.Namespace,
) -> tuple[str, dict[str, Any]]:
    """Fetch appearance, seed assets, reserve a slot, and enqueue one job.

    Returns (job_id, queue_payload).
    """
    appearance = tryon.fetch_character_flashvars(username, timeout=15)
    seed_missing_assets(
        outputs,
        appearance,
        dataset_version=args.dataset_version,
    )
    job_id = str(uuid4())
    request = JobRequest(
        job_id=job_id,
        created_at=utc_now(),
        discord=DiscordTarget(
            user_id=args.user_id,
            channel_id=args.channel_id,
            guild_id=args.guild_id,
        ),
        render=RenderSettings(
            username=username,
            max_frames=args.max_frames,
            raster_size=args.output_size * 2,
            output_size=args.output_size,
            webp_quality=args.webp_quality,
            webp_method=args.webp_method,
            webp_lossless=args.webp_lossless or None,
            raster_backend=args.raster_backend,
        ),
        appearance=appearance,
    )
    jobs = JobStore(outputs["JobTableName"])
    jobs.acquire(request, args.max_active)
    # Match the public bot contract: a sparse render block the launcher hydrates.
    queue_payload = request.to_dict()
    queue_payload["render"] = {
        "username": request.render.username,
        "max_frames": request.render.max_frames,
        "raster_size": request.render.raster_size,
        "output_size": request.render.output_size,
        "webp_quality": request.render.webp_quality,
        "webp_method": request.render.webp_method,
        "webp_lossless": request.render.webp_lossless,
        "raster_backend": request.render.raster_backend,
    }
    sqs = boto3.client("sqs")
    try:
        sqs.send_message(
            QueueUrl=outputs["JobQueueUrl"],
            MessageBody=json.dumps(queue_payload, separators=(",", ":"), sort_keys=True),
        )
    except Exception as error:
        try:
            jobs.release(
                job_id,
                "FAILED",
                attributes={"error_code": f"SUBMIT_QUEUE_SEND_FAILED:{type(error).__name__}"},
            )
        except Exception:  # noqa: BLE001, S110 - preserve the enqueue error.
            pass
        raise
    return job_id, queue_payload


def wait_for(
    outputs: dict[str, str],
    job_id: str,
    *,
    timeout_seconds: int,
    poll_seconds: float,
) -> dict[str, Any]:
    """Poll the job table until `job_id` reaches a terminal state."""
    jobs = JobStore(outputs["JobTableName"])
    deadline = time.monotonic() + timeout_seconds
    previous: str | None = None
    while time.monotonic() < deadline:
        record = jobs.get(job_id)
        status = str((record or {}).get("status", "MISSING"))
        if status != previous:
            print(f"  {job_id[:8]} status -> {status}", flush=True)
            previous = status
        if status in TERMINAL_STATUSES:
            return record or {}
        time.sleep(poll_seconds)
    raise TimeoutError(f"Timed out waiting for {job_id} to reach a terminal state")


def result_row(
    outputs: dict[str, str],
    job_id: str,
    record: dict[str, Any],
    args: argparse.Namespace,
) -> dict[str, Any]:
    """Build the final per-job result row (optionally verifying the WebP)."""
    payload = record.get("result_payload")
    if not isinstance(payload, dict):
        return {"job_id": job_id, "status": str(record.get("status")), "no_result_payload": True}
    result = payload.get("result")
    if not isinstance(result, dict) or not isinstance(result.get("url"), str):
        return {
            "job_id": job_id,
            "status": str(payload.get("status")),
            "error": json.dumps(payload, sort_keys=True, default=str),
        }
    row: dict[str, Any] = {
        "job_id": job_id,
        "status": str(payload.get("status")),
        "url": result["url"],
        "width": result.get("width"),
        "height": result.get("height"),
        "frame_count": result.get("frame_count"),
        "bytes": result.get("bytes"),
        "cache_hit": result.get("cache_hit"),
    }
    if not args.no_verify:
        try:
            verified = verify_webp(result["url"], outputs["CloudFrontBaseUrl"])
            row.update(verified)
        except Exception as error:  # noqa: BLE001 - verification is best-effort.
            row["verify_error"] = str(error)
    return row


def main() -> int:
    args = parser().parse_args()
    if args.max_active < 1 or not 1 <= args.max_frames <= 2000:
        raise SystemExit("--max-active and --max-frames must be positive/within range")
    usernames = args.usernames or ["artix"]
    if any(not 1 <= len(name) <= 25 for name in usernames):
        raise SystemExit("Usernames must be between 1 and 25 characters")

    outputs = load_outputs(args.outputs.resolve())
    results: list[dict[str, Any]] = []
    for username in usernames:
        print(f"\nSubmitting {username!r} ...", flush=True)
        job_id, _payload = queue_one(outputs, username, args)
        print(
            json.dumps(
                {
                    "event": "queued",
                    "job_id": job_id,
                    "username": username,
                    "render": _payload["render"],
                },
                sort_keys=True,
            ),
            flush=True,
        )
        if not args.watch:
            results.append({"job_id": job_id, "username": username, "queued": True})
            continue
        record = wait_for(
            outputs,
            job_id,
            timeout_seconds=args.timeout_seconds,
            poll_seconds=args.poll_seconds,
        )
        row = result_row(outputs, job_id, record, args)
        row["username"] = username
        results.append(row)
        print(json.dumps({"result": row}, indent=2, sort_keys=True, default=str), flush=True)

    print("\n=== SUMMARY ===")
    print("| username | job_id | status | bytes | size | url |")
    print("|---|---|---|---|---|---|")
    for row in results:
        size = (
            f"{row['width']}x{row['height']}" if row.get("width") else "?"
        )
        print(
            f"| {row.get('username', '?')} | {row['job_id'][:8]} | {row.get('status', '?')} "
            f"| {row.get('bytes', row.get('content_length', '?'))} | {size} | {row.get('url', row.get('error', '?'))} |"
        )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())