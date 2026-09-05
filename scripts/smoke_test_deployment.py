"""Submit one real render through the deployed SQS workflow and verify its CDN result."""

from __future__ import annotations

import argparse
import json
import tempfile
import time
import urllib.request
from pathlib import Path
from typing import Any
from uuid import uuid4

import boto3
from aqw_char_renderer import character_svg
from aqw_char_renderer.contracts import (
    DiscordTarget,
    JobRequest,
    RenderSettings,
    utc_now,
)
from aqw_char_renderer.jobs import TERMINAL_STATUSES, JobStore
from aqw_char_renderer.legacy import preview_aqw_tryon as tryon
from aqw_char_renderer.source_assets import SourceAssetCatalog, SourceAssetError
from aqw_char_renderer.storage import S3ObjectStore


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser(
        description="Queue a deployed AQW character render and validate its CloudFront WebP."
    )
    result.add_argument("--outputs", type=Path, default=Path("cdk-outputs.dev.json"))
    result.add_argument("--username", default="artix")
    result.add_argument("--user-id", default="900000000000000001")
    result.add_argument("--channel-id", default="900000000000000002")
    result.add_argument("--guild-id", default="900000000000000003")
    result.add_argument("--max-active", type=int, default=2)
    result.add_argument("--dataset-version", default="dev-v1")
    result.add_argument("--max-frames", type=int, default=8)
    result.add_argument(
        "--output-size",
        type=int,
        choices=(256, 512, 1024, 2048),
        default=256,
        help="Final longest dimension; frames rasterize at twice this size",
    )
    result.add_argument(
        "--webp-quality",
        type=float,
        default=85.0,
        help="cwebp quality factor 0..100 (default 85)",
    )
    result.add_argument(
        "--webp-method",
        type=int,
        default=4,
        help="cwebp method 0..6 (default 4)",
    )
    result.add_argument(
        "--webp-lossless",
        action="store_true",
        help="Encode frames with cwebp -lossless 1 instead of lossy",
    )
    result.add_argument(
        "--raster-backend",
        choices=("resvg", "thorvg"),
        default="resvg",
        help="SVG rasterizer for the component pass: resvg (pinned 0.48.1, default) or thorvg (1.1.1)",
    )
    result.add_argument("--timeout-seconds", type=int, default=1_200)
    result.add_argument("--poll-seconds", type=float, default=5)
    return result


def load_outputs(path: Path) -> dict[str, str]:
    document = json.loads(path.read_text())
    if not isinstance(document, dict) or len(document) != 1:
        raise TypeError(f"Expected exactly one stack in {path}")
    outputs = next(iter(document.values()))
    if not isinstance(outputs, dict):
        raise TypeError(f"Malformed CDK outputs in {path}")
    required = {
        "CloudFrontBaseUrl",
        "JobQueueUrl",
        "JobTableName",
        "RenderEnabledParameterName",
        "ResultQueueUrl",
        "SourceAssetBucketName",
    }
    missing = sorted(required.difference(outputs))
    if missing:
        raise RuntimeError(f"Missing CDK output(s): {', '.join(missing)}")
    return {str(key): str(value) for key, value in outputs.items()}


def seed_missing_assets(
    outputs: dict[str, str],
    appearance: dict[str, str],
    *,
    dataset_version: str,
) -> list[str]:
    store = S3ObjectStore()
    bucket = outputs["SourceAssetBucketName"]
    catalog = SourceAssetCatalog(
        store.read_json(bucket, f"datasets/{dataset_version}/manifest.json")
    )
    if catalog.dataset_version != dataset_version:
        raise RuntimeError("Smoke dataset version does not match its manifest")
    missing: list[str] = []
    for asset in character_svg.appearance_assets(appearance).values():
        try:
            catalog.get(asset.remote_path)
        except SourceAssetError:
            missing.append(asset.remote_path)
    if not missing:
        return []
    with tempfile.TemporaryDirectory(prefix="aqw-char-smoke-assets-") as temporary:
        root = Path(temporary)
        for remote_path in missing:
            catalog.resolve_and_download(
                remote_path,
                store=store,
                bucket=bucket,
                root=root,
                allow_official_fallback=True,
                timeout=15,
            )
            print(json.dumps({"event": "seeded_asset", "path": remote_path}))
    return missing


def receive_result(
    sqs: Any,
    queue_url: str,
    job_id: str,
    deadline: float,
) -> tuple[dict[str, Any], str]:
    while time.monotonic() < deadline:
        response = sqs.receive_message(
            QueueUrl=queue_url,
            MaxNumberOfMessages=10,
            WaitTimeSeconds=10,
            VisibilityTimeout=30,
        )
        for message in response.get("Messages", []):
            try:
                payload = json.loads(message["Body"])
            except (KeyError, TypeError, json.JSONDecodeError):
                payload = None
            if isinstance(payload, dict) and payload.get("job_id") == job_id:
                return payload, str(message["ReceiptHandle"])
            sqs.change_message_visibility(
                QueueUrl=queue_url,
                ReceiptHandle=message["ReceiptHandle"],
                VisibilityTimeout=0,
            )
    raise TimeoutError(f"Timed out waiting for result message for {job_id}")


def verify_webp(url: str, expected_base_url: str) -> dict[str, Any]:
    expected_prefix = expected_base_url.rstrip("/") + "/renders/"
    if not url.startswith(expected_prefix):
        raise RuntimeError(f"Result URL is outside the expected CDN prefix: {url}")
    request = urllib.request.Request(url, headers={"User-Agent": "aqw-char-smoke/1"})
    with urllib.request.urlopen(request, timeout=30) as response:
        prefix = response.read(16)
        content_type = response.headers.get_content_type()
        content_length = response.headers.get("Content-Length")
        status = response.status
    if not (prefix.startswith(b"RIFF") and prefix[8:12] == b"WEBP"):
        raise RuntimeError("CloudFront response is not a WebP file")
    return {
        "url": url,
        "http_status": status,
        "content_type": content_type,
        "content_length": int(content_length) if content_length else None,
    }


def main() -> int:
    args = parser().parse_args()
    if (
        args.max_active < 1
        or not 1 <= args.max_frames <= 2000
        or args.timeout_seconds < 1
        or args.poll_seconds <= 0
    ):
        raise SystemExit("Limits and timeouts must be positive")
    outputs = load_outputs(args.outputs.resolve())
    ssm = boto3.client("ssm")
    enabled = ssm.get_parameter(Name=outputs["RenderEnabledParameterName"])["Parameter"][
        "Value"
    ]
    if enabled.casefold() != "true":
        raise RuntimeError("The deployed render safety switch is disabled")

    appearance = tryon.fetch_character_flashvars(args.username, timeout=15)
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
            username=args.username,
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
    # Match the public bot contract while keeping deployment smoke tests short.
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
        jobs.release(
            job_id,
            "FAILED",
            attributes={"error_code": f"SMOKE_QUEUE_SEND_FAILED:{type(error).__name__}"},
        )
        raise
    print(json.dumps({"event": "queued", "job_id": job_id, "username": args.username}))

    deadline = time.monotonic() + args.timeout_seconds
    previous_status: str | None = None
    while time.monotonic() < deadline:
        record = jobs.get(job_id)
        status = str((record or {}).get("status", "MISSING"))
        if status != previous_status:
            print(json.dumps({"event": "status", "job_id": job_id, "status": status}))
            previous_status = status
        if status in TERMINAL_STATUSES:
            break
        time.sleep(args.poll_seconds)
    else:
        raise TimeoutError(f"Timed out waiting for terminal job state for {job_id}")

    # The Discord bot is a competing consumer on the shared result queue, so
    # a smoke process cannot reliably receive its own message. Completion
    # atomically persists the exact payload in the job record before enqueueing
    # it; use that durable copy and verify that queue publication was recorded.
    terminal = jobs.get(job_id) or {}
    payload = terminal.get("result_payload")
    if not isinstance(payload, dict):
        raise TypeError(f"Job has no persisted result payload: {json.dumps(terminal, default=str)}")
    if not terminal.get("result_enqueued_at"):
        raise RuntimeError("Job completed without recording result-queue publication")
    if payload.get("status") != "SUCCEEDED":
        raise RuntimeError(f"Render failed: {json.dumps(payload, sort_keys=True, default=str)}")
    result = payload.get("result")
    if not isinstance(result, dict) or not isinstance(result.get("url"), str):
        raise TypeError(f"Malformed success result: {json.dumps(payload, sort_keys=True)}")
    dimensions = (int(result.get("width", 0)), int(result.get("height", 0)))
    if max(dimensions) != args.output_size:
        raise RuntimeError(
            f"Expected {args.output_size}px output, got {dimensions[0]}x{dimensions[1]}"
        )
    verified = verify_webp(result["url"], outputs["CloudFrontBaseUrl"])
    summary = {
        "event": "verified",
        "job_id": job_id,
        "username": request.render.username,
        **result,
        **verified,
    }
    print(json.dumps(summary, indent=2, sort_keys=True, default=str))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
