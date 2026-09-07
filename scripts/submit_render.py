"""Queue AQW character renders through the deployed workflow and wait for them.

A thin, ergonomic wrapper around the same SQS / DynamoDB admission path the
Discord bot uses: it fetches the character's equipped flashvars, seeds any
missing source assets, reserves a per-user slot, enqueues the render job, and
polls the job table until each job reaches a terminal state. It prints one
compact result row per character and (optionally) verifies the delivered WebP
on CloudFront. CLI successes and failures never post to Discord.

Examples:
    # The short wrapper supplies uv and the dev AWS profile/Region.
    scripts/render-character artix

    # One character with render and WebP controls.
    scripts/render-character alina -s 1024 -n 30 -q 70 -m 2 \\
        --facing left --zoom 1 --padding 8

    # Multiple characters in one run (each gets its own job)
    scripts/render-character alina godlow --lossless

    # Queue without waiting
    scripts/render-character alina --no-watch

    # Force a true cold-path benchmark through every cross-job stage.
    scripts/render-character mck -s 1024 --bounds-mode inline \
        --component-raster-mode inline --no-cache
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
    CacheSettings,
    DiscordTarget,
    ItemOverride,
    JobRequest,
    RenderSettings,
    utc_now,
)
from aqw_char_renderer.jobs import TERMINAL_STATUSES, JobStore
from aqw_char_renderer.legacy import preview_aqw_tryon as tryon

# Proven helpers from the smoke harness (same repo, importable module).
from smoke_test_deployment import load_outputs, seed_missing_assets, verify_image


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser(
        description="Queue deployed AQW character renders (with WebP toggles) and watch them.",
        epilog="Restart with the same appearance/settings and a new job ID: scripts/render-character --restart JOB_ID (must come first; --restart --help for options).",
    )
    result.add_argument("usernames", nargs="*", help="Public AQW character name(s)")
    result.add_argument("--outputs", type=Path, default=Path("cdk-outputs.dev.json"))
    result.add_argument(
        "-s",
        "--output-size",
        type=int,
        choices=(256, 512, 1024, 2048),
        default=2048,
        help="final longest dimension (default: 2048)",
    )
    result.add_argument(
        "--raster-size",
        type=int,
        help="raster longest dimension, 64..4096 (default: 2x output size)",
    )
    result.add_argument(
        "--zoom", type=float, default=1.0, help="FFDec zoom, 0.25..8 (default: 1)"
    )
    result.add_argument(
        "--padding", type=int, default=0, help="final transparent padding in pixels"
    )
    result.add_argument("--facing", choices=("left", "right"), default="right")
    result.add_argument(
        "--base-items", action="store_true", help="render base equipped items"
    )
    result.add_argument(
        "--show-hidden", action="store_true", help="include hidden equipped items"
    )
    result.add_argument(
        "--complete-loop",
        action=argparse.BooleanOptionalAction,
        default=True,
        help="complete an animation loop (use --no-complete-loop to disable)",
    )
    result.add_argument(
        "--subframe-start",
        "--start-frame",
        dest="subframe_start",
        type=int,
        default=1,
        help="one-based source frame at which to start (default: 1)",
    )
    result.add_argument(
        "--item-id",
        "--override-item-id",
        dest="override_item_id",
        type=int,
        help="temporarily equip an AQW item ID",
    )
    result.add_argument(
        "--slot",
        "--override-slot",
        dest="override_slot",
        choices=("armor", "weapon", "helm", "cape", "ground"),
        help="slot for --item-id when it cannot be inferred",
    )
    result.add_argument("--background", action=argparse.BooleanOptionalAction, default=None)
    result.add_argument("--info", action=argparse.BooleanOptionalAction, default=None)
    result.add_argument("--framing", choices=("content", "fixed"))
    result.add_argument("--viewport", type=float, nargs=4, metavar=("X", "Y", "WIDTH", "HEIGHT"))
    result.add_argument("--character-position", type=float, nargs=2, metavar=("X", "Y"))
    result.add_argument("--view", choices=("character", "charpage"), default="character", help="Transparent character or full charpage card")
    result.add_argument("--format", dest="output_format", choices=("webp", "avif"), default="webp")
    result.add_argument("--rgba-compression", choices=("zstd", "none"), default="zstd", help="AVIF intermediate compression; zstd level 1 is lossless")
    result.add_argument("--avif-quality", type=int, choices=range(101), default=70, metavar="0..100")
    result.add_argument("--avif-speed", type=int, choices=range(11), default=8, metavar="0..10")
    result.add_argument(
        "-q",
        "--webp-quality",
        type=float,
        default=85.0,
        help="cwebp -q 0..100 (default: 85)",
    )
    result.add_argument(
        "-m",
        "--webp-method",
        type=int,
        choices=range(7),
        default=4,
        help="cwebp -m 0..6 (default: 4)",
    )
    result.add_argument(
        "--raster-backend",
        choices=("resvg",),
        default="resvg",
        help="SVG rasterizer (the deployed pipeline is resvg-only)",
    )
    result.add_argument(
        "--webp-lossless",
        "--lossless",
        action="store_true",
        help="Use lossless encoding for the selected output format",
    )
    result.add_argument(
        "-n",
        "--max-frames",
        type=int,
        default=120,
        help="animation frame count (default: 120)",
    )
    result.add_argument(
        "--bounds-mode",
        choices=("inline", "distributed"),
        default="inline",
        help="bounds fan-out mode selected for this job (default: inline)",
    )
    result.add_argument(
        "--component-raster-mode",
        choices=("inline", "distributed"),
        default="inline",
        help="component-raster fan-out mode selected for this job (default: inline)",
    )
    cache = result.add_argument_group("cache control")
    cache.add_argument(
        "--no-cache",
        action="store_true",
        help="bypass every cross-job compute cache",
    )
    cache.add_argument(
        "--no-render-cache",
        action="store_true",
        help="bypass a completed final-render cache hit",
    )
    cache.add_argument(
        "--no-animation-cache",
        action="store_true",
        help="ignore cached animation-loop metadata",
    )
    cache.add_argument(
        "--no-vector-cache",
        action="store_true",
        help="rerun FFDec instead of reusing vector manifests or metadata",
    )
    cache.add_argument(
        "--no-bounds-cache",
        action="store_true",
        help="recompute SVG bounds into job-scoped result objects",
    )
    cache.add_argument(
        "--no-component-cache",
        action="store_true",
        help="rerasterize appearance-independent component states",
    )
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
        help="Skip the CloudFront image fetch/validation at the end",
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
    override = (
        ItemOverride(item_id=args.override_item_id, slot=args.override_slot)
        if args.override_item_id is not None
        else None
    )
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
            base_items=args.base_items,
            show_hidden=args.show_hidden,
            facing=args.facing,
            override=override,
            complete_loop=args.complete_loop,
            max_frames=args.max_frames,
            subframe_start=args.subframe_start,
            zoom=args.zoom,
            raster_size=args.raster_size,
            output_size=args.output_size,
            padding=args.padding,
            view=args.view,
            presentation={key: value for key, value in {
                "background": args.background, "info": args.info, "framing": args.framing,
                "viewport": args.viewport, "character_position": args.character_position,
            }.items() if value is not None},
            output_format=args.output_format,
            rgba_compression=args.rgba_compression,
            avif_quality=args.avif_quality,
            avif_speed=args.avif_speed,
            webp_quality=args.webp_quality,
            webp_method=args.webp_method,
            webp_lossless=args.webp_lossless or None,
            raster_backend=args.raster_backend,
        ),
        bounds_mode=args.bounds_mode,
        component_raster_mode=args.component_raster_mode,
        cache=CacheSettings(
            render=not (args.no_cache or args.no_render_cache),
            animation=not (args.no_cache or args.no_animation_cache),
            vectors=not (args.no_cache or args.no_vector_cache),
            bounds=not (args.no_cache or args.no_bounds_cache),
            components=not (args.no_cache or args.no_component_cache),
        ),
        appearance=appearance,
    )
    return enqueue_request(outputs, request, args.max_active)


def enqueue_request(
    outputs: dict[str, str], request: JobRequest, maximum_active: int
) -> tuple[str, dict[str, Any]]:
    """Shared CLI admission/SQS path; fresh renders and restarts never notify Discord."""
    jobs = JobStore(outputs["JobTableName"])
    jobs.acquire(request, maximum_active)
    job_id = request.job_id
    # Keep this CLI deterministic: every exposed flag is included explicitly,
    # independently of fleet defaults used to hydrate sparse Discord requests.
    queue_payload = request.to_dict()
    sqs = boto3.client("sqs")
    try:
        sqs.send_message(
            QueueUrl=outputs["JobQueueUrl"],
            MessageBody=json.dumps(
                queue_payload, separators=(",", ":"), sort_keys=True
            ),
        )
    except Exception as error:
        try:
            jobs.release(
                job_id,
                "FAILED",
                attributes={
                    "error_code": f"SUBMIT_QUEUE_SEND_FAILED:{type(error).__name__}"
                },
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
        return {
            "job_id": job_id,
            "status": str(record.get("status")),
            "no_result_payload": True,
        }
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
            verified = verify_image(result["url"], outputs["CloudFrontBaseUrl"], result.get("output_format", "webp"))
            row.update(verified)
        except Exception as error:  # noqa: BLE001 - verification is best-effort.
            row["verify_error"] = str(error)
    return row


def main() -> int:
    args = parser().parse_args()
    args.raster_size = args.raster_size or args.output_size * 2
    if args.max_active < 1 or not 1 <= args.max_frames <= 2000:
        raise SystemExit(
            "--max-active must be positive and --max-frames must be 1..2000"
        )
    if not 64 <= args.raster_size <= 4096 or args.output_size > args.raster_size:
        raise SystemExit("--raster-size must be 64..4096 and at least --output-size")
    if not 0.25 <= args.zoom <= 8:
        raise SystemExit("--zoom must be 0.25..8")
    if not 0 <= args.webp_quality <= 100:
        raise SystemExit("--webp-quality must be 0..100")
    if not 1 <= args.subframe_start <= 10_000:
        raise SystemExit("--subframe-start must be 1..10000")
    if not 0 <= args.padding <= 1023 or args.padding * 2 >= args.output_size:
        raise SystemExit("--padding must be 0..1023 and less than half --output-size")
    if args.override_slot is not None and args.override_item_id is None:
        raise SystemExit("--slot requires --item-id")
    if (
        args.override_item_id is not None
        and not 1 <= args.override_item_id <= 10_000_000
    ):
        raise SystemExit("--item-id must be 1..10000000")
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
                    "bounds_mode": _payload["bounds_mode"],
                    "component_raster_mode": _payload["component_raster_mode"],
                    "render": _payload["render"],
                    "cache": _payload.get(
                        "cache",
                        {
                            "render": True,
                            "animation": True,
                            "vectors": True,
                            "bounds": True,
                            "components": True,
                        },
                    ),
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
        print(
            json.dumps({"result": row}, indent=2, sort_keys=True, default=str),
            flush=True,
        )

    print("\n=== SUMMARY ===")
    print("| username | job_id | status | bytes | size | url |")
    print("|---|---|---|---|---|---|")
    for row in results:
        size = f"{row['width']}x{row['height']}" if row.get("width") else "?"
        print(
            f"| {row.get('username', '?')} | {row['job_id'][:8]} | {row.get('status', '?')} "
            f"| {row.get('bytes', row.get('content_length', '?'))} | {size} | {row.get('url', row.get('error', '?'))} |"
        )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
