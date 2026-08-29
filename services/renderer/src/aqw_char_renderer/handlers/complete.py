"""Idempotent terminal job release and result-queue publication."""

from __future__ import annotations

import json
from typing import Any

import boto3

from aqw_char_renderer.config import RuntimeConfig
from aqw_char_renderer.contracts import JobRequest
from aqw_char_renderer.jobs import JobStore
from aqw_char_renderer.structured_logging import log_event


def _safe_failure(value: Any) -> tuple[str, str]:
    error = "RENDER_FAILED"
    message = "The character could not be rendered. Please try again later."
    if isinstance(value, dict):
        raw_error = str(value.get("Error") or "")
        if raw_error.endswith("CharacterSvgError"):
            error = "CHARACTER_RENDER_ERROR"
            message = "That character or one of its equipped items could not be rendered."
    return error, message


def _publish(config: RuntimeConfig, payload: dict[str, Any]) -> None:
    boto3.client("sqs").send_message(
        QueueUrl=config.result_queue_url,
        MessageBody=json.dumps(payload, separators=(",", ":"), sort_keys=True),
    )


def success_handler(event: dict[str, Any], _context: Any) -> dict[str, Any]:
    config = RuntimeConfig.from_env()
    request = JobRequest.from_dict(event["request"])
    result = event["result"]
    status = "CACHE_HIT" if result.get("cache_hit") else "SUCCEEDED"
    payload = {
        "schema_version": 1,
        "job_id": request.job_id,
        "status": "SUCCEEDED",
        "discord": {
            "user_id": request.discord.user_id,
            "guild_id": request.discord.guild_id,
            "channel_id": request.discord.channel_id,
        },
        "result": {
            key: result[key]
            for key in (
                "url",
                "frame_count",
                "width",
                "height",
                "duration_ms",
                "bytes",
                "cache_hit",
            )
        },
    }
    jobs = JobStore(config.job_table)
    released = jobs.release(
        request.job_id,
        status,
        attributes={
            "render_hash": result.get("render_hash", event.get("render_hash", "")),
            "result_url": result["url"],
            "result_payload": payload,
        },
    )
    current = jobs.get(request.job_id) or {}
    if not current.get("result_enqueued_at"):
        _publish(config, payload)
        jobs.mark_result_enqueued(request.job_id)
    log_event(
        "job_succeeded", job_id=request.job_id, cache_hit=result.get("cache_hit"), released=released
    )
    return {"job_id": request.job_id, "status": "SUCCEEDED", "released": released}


def failure_handler(event: dict[str, Any], _context: Any) -> dict[str, Any]:
    config = RuntimeConfig.from_env()
    request = JobRequest.from_dict(event["request"])
    code, message = _safe_failure(event.get("failure"))
    payload = {
        "schema_version": 1,
        "job_id": request.job_id,
        "status": "FAILED",
        "discord": {
            "user_id": request.discord.user_id,
            "guild_id": request.discord.guild_id,
            "channel_id": request.discord.channel_id,
        },
        "error": {"code": code, "message": message},
    }
    jobs = JobStore(config.job_table)
    existing = jobs.get(request.job_id) or {}
    if existing.get("status") in {"CACHE_HIT", "SUCCEEDED"}:
        log_event(
            "failure_after_success_ignored",
            job_id=request.job_id,
            existing_status=existing.get("status"),
        )
        return {
            "job_id": request.job_id,
            "status": existing["status"],
            "released": False,
        }
    released = jobs.release(
        request.job_id,
        "FAILED",
        attributes={"error_code": code, "result_payload": payload},
    )
    current = jobs.get(request.job_id) or {}
    if not current.get("result_enqueued_at"):
        _publish(config, payload)
        jobs.mark_result_enqueued(request.job_id)
    log_event("job_failed", job_id=request.job_id, error_code=code, released=released)
    return {"job_id": request.job_id, "status": "FAILED", "released": released}


def handler(event: dict[str, Any], context: Any) -> dict[str, Any]:
    if "failure" in event:
        return failure_handler(event, context)
    return success_handler(event, context)
