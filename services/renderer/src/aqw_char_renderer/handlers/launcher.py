"""SQS launcher for idempotent Step Functions Standard executions."""

from __future__ import annotations

import json
import os
from collections.abc import Mapping
from typing import Any

import boto3
from botocore.exceptions import ClientError

from aqw_char_renderer.config import RuntimeConfig
from aqw_char_renderer.contracts import JobRequest
from aqw_char_renderer.hashing import canonical_json
from aqw_char_renderer.jobs import TERMINAL_STATUSES, JobStore
from aqw_char_renderer.structured_logging import log_event


def _execution_arn(state_machine_arn: str, job_id: str) -> str:
    return f"{state_machine_arn.replace(':stateMachine:', ':execution:')}:{job_id}"


def hydrate_request_defaults(value: Any, config: RuntimeConfig) -> dict[str, Any]:
    """Apply centrally configured render defaults without overriding callers."""
    if not isinstance(value, Mapping):
        return value
    payload = dict(value)
    raw_render = payload.get("render")
    if not isinstance(raw_render, Mapping):
        return payload
    render = dict(raw_render)
    # Normalize the pre-v17 size field before injecting the new defaults so
    # already-queued requests retain their original one-size behavior.
    if "max_size" in render and not {
        "raster_size",
        "output_size",
    }.intersection(render):
        legacy_size = render.pop("max_size")
        render["raster_size"] = legacy_size
        render["output_size"] = legacy_size
    for name, default in config.render_defaults().items():
        render.setdefault(name, default)
    payload["render"] = render
    return payload


def handler(event: dict[str, Any], _context: Any) -> dict[str, Any]:
    state_machine_arn = os.environ["CHAR_RENDER_STATE_MACHINE_ARN"]
    config = RuntimeConfig.from_env()
    step_functions = boto3.client("stepfunctions")
    jobs = JobStore(config.job_table)
    records = event.get("Records")
    if not isinstance(records, list) or len(records) != 1:
        raise ValueError("Launcher requires exactly one SQS record")
    request = JobRequest.from_dict(
        hydrate_request_defaults(json.loads(records[0]["body"]), config)
    )
    admitted = jobs.get(request.job_id)
    if admitted is None:
        raise RuntimeError(f"Job {request.job_id} was not admitted through DynamoDB")
    if admitted.get("slot_released") is True or admitted.get("status") in TERMINAL_STATUSES:
        log_event(
            "released_job_skipped",
            job_id=request.job_id,
            discord_user_id=request.discord.user_id,
            status=admitted.get("status"),
        )
        return {
            "job_id": request.job_id,
            "status": str(admitted.get("status") or "UNKNOWN"),
            "skipped": True,
        }
    execution_input = json.dumps(
        {"request": request.to_dict()}, separators=(",", ":"), sort_keys=True
    )
    try:
        response = step_functions.start_execution(
            stateMachineArn=state_machine_arn,
            name=request.job_id,
            input=execution_input,
        )
        execution_arn = response["executionArn"]
        duplicate = False
    except ClientError as error:
        if error.response.get("Error", {}).get("Code") != "ExecutionAlreadyExists":
            raise
        execution_arn = _execution_arn(state_machine_arn, request.job_id)
        existing = step_functions.describe_execution(executionArn=execution_arn)
        try:
            existing_request = json.loads(existing["input"])["request"]
        except (KeyError, TypeError, json.JSONDecodeError) as parse_error:
            raise RuntimeError("Existing execution input cannot be verified") from parse_error
        if canonical_json(existing_request) != canonical_json(request.to_dict()):
            raise RuntimeError("Existing execution name belongs to a different request")
        duplicate = True
    jobs.mark_execution(request.job_id, execution_arn)
    log_event(
        "execution_started",
        job_id=request.job_id,
        discord_user_id=request.discord.user_id,
        execution_arn=execution_arn,
        duplicate=duplicate,
    )
    return {"job_id": request.job_id, "execution_arn": execution_arn, "duplicate": duplicate}
