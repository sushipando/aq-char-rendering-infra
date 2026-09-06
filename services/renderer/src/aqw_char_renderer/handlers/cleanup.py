"""Best-effort terminal-state cleanup for Step Functions status events."""

from __future__ import annotations

import json
from datetime import UTC, datetime, timedelta
from typing import Any

import boto3

from aqw_char_renderer.config import RuntimeConfig
from aqw_char_renderer.jobs import JobStore, discord_notification_pending
from aqw_char_renderer.structured_logging import log_event

_STATUS_MAP = {"FAILED": "FAILED", "TIMED_OUT": "TIMED_OUT", "ABORTED": "ABORTED"}


def _publish(config: RuntimeConfig, payload: dict[str, Any]) -> None:
    boto3.client("sqs").send_message(
        QueueUrl=config.result_queue_url,
        MessageBody=json.dumps(payload, separators=(",", ":"), sort_keys=True),
    )


def _failure_payload(record: dict[str, Any], status: str) -> dict[str, Any]:
    return {
        "schema_version": 1,
        "job_id": record["job_id"],
        "status": "FAILED",
        "discord": {
            "user_id": record["user_id"],
            "guild_id": record.get("guild_id") or None,
            "channel_id": record["channel_id"],
        },
        "error": {
            "code": f"WORKFLOW_{status}",
            "message": "The character render stopped before it completed. Please try again.",
        },
    }


def _release_failure(
    jobs: JobStore,
    config: RuntimeConfig,
    record: dict[str, Any],
    status: str,
) -> bool:
    payload = _failure_payload(record, status)
    released = jobs.release(
        record["job_id"],
        status,
        attributes={"error_code": f"WORKFLOW_{status}", "result_payload": payload},
    )
    current = jobs.get(record["job_id"]) or {}
    if discord_notification_pending(current):
        _publish(config, current["result_payload"])
        jobs.mark_result_enqueued(record["job_id"])
    return released


def _scheduled_reconcile(config: RuntimeConfig, jobs: JobStore) -> dict[str, Any]:
    step_functions = boto3.client("stepfunctions")
    repaired: list[str] = []
    requeued: list[str] = []
    stale_before = datetime.now(UTC) - timedelta(hours=1)
    for record in jobs.scan_reconcilable():
        job_id = str(record.get("job_id") or "")
        if not job_id:
            continue
        if discord_notification_pending(record):
            _publish(config, record["result_payload"])
            jobs.mark_result_enqueued(job_id)
            requeued.append(job_id)
        if record.get("slot_released") is True:
            continue
        status = str(record.get("status") or "QUEUED")
        execution_arn = str(record.get("execution_arn") or "")
        terminal: str | None = None
        if execution_arn:
            try:
                execution = step_functions.describe_execution(executionArn=execution_arn)
            except step_functions.exceptions.ExecutionDoesNotExist:
                terminal = "FAILED"
            else:
                terminal = _STATUS_MAP.get(str(execution.get("status") or ""))
        elif status == "QUEUED":
            try:
                updated = datetime.fromisoformat(str(record["updated_at"]))
            except (KeyError, ValueError):
                updated = stale_before - timedelta(seconds=1)
            if updated < stale_before:
                terminal = "FAILED"
        if terminal and _release_failure(jobs, config, record, terminal):
            repaired.append(job_id)
    log_event("scheduled_reconciliation", repaired=len(repaired), requeued=len(requeued))
    return {"scheduled": True, "repaired": repaired, "requeued": requeued}


def handler(event: dict[str, Any], _context: Any) -> dict[str, Any]:
    config = RuntimeConfig.from_env()
    jobs = JobStore(config.job_table)
    if event.get("source") == "aws.events" and event.get("detail-type") == "Scheduled Event":
        return _scheduled_reconcile(config, jobs)
    detail = event.get("detail") or {}
    execution_arn = str(detail.get("executionArn") or "")
    job_id = str(detail.get("name") or execution_arn.rsplit(":", 1)[-1])
    status = _STATUS_MAP.get(str(detail.get("status") or ""))
    if not job_id or status is None:
        log_event("cleanup_ignored", event_source=event.get("source"))
        return {"ignored": True}
    record = jobs.get(job_id)
    if record is None:
        return {"job_id": job_id, "missing": True}
    released = _release_failure(jobs, config, record, status)
    log_event("workflow_cleanup", job_id=job_id, status=status, released=released)
    return {"job_id": job_id, "status": status, "released": released}
