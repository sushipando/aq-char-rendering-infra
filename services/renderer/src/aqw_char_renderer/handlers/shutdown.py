"""Budget-triggered rendering kill switch that preserves all stored data."""

from __future__ import annotations

import json
import os
from typing import Any

import boto3
from botocore.exceptions import ClientError

from aqw_char_renderer.structured_logging import log_event


def handler(_event: dict[str, Any], _context: Any) -> dict[str, Any]:
    lambda_client = boto3.client("lambda")
    step_functions = boto3.client("stepfunctions")
    ssm = boto3.client("ssm")
    parameter = os.environ["CHAR_RENDER_ENABLED_PARAMETER"]
    state_machine_arn = os.environ["CHAR_RENDER_STATE_MACHINE_ARN"]
    function_names = json.loads(os.environ.get("CHAR_RENDER_STOP_FUNCTIONS", "[]"))
    event_source_uuids = json.loads(os.environ.get("CHAR_RENDER_LAUNCHER_EVENT_SOURCE_UUIDS", "[]"))
    ssm.put_parameter(Name=parameter, Value="false", Type="String", Overwrite=True)
    disabled_mappings: list[str] = []
    for uuid in event_source_uuids:
        try:
            lambda_client.update_event_source_mapping(UUID=uuid, Enabled=False)
            disabled_mappings.append(uuid)
        except ClientError as error:
            if error.response.get("Error", {}).get("Code") != "ResourceInUseException":
                raise
    throttled: list[str] = []
    for function_name in function_names:
        lambda_client.put_function_concurrency(
            FunctionName=function_name, ReservedConcurrentExecutions=0
        )
        throttled.append(function_name)
    stopped: list[str] = []
    paginator = step_functions.get_paginator("list_executions")
    for page in paginator.paginate(stateMachineArn=state_machine_arn, statusFilter="RUNNING"):
        for execution in page.get("executions", []):
            execution_arn = execution["executionArn"]
            step_functions.stop_execution(
                executionArn=execution_arn,
                error="BudgetShutdown",
                cause="Automatic render-compute shutdown triggered by AWS Budget",
            )
            stopped.append(execution_arn)
    result = {
        "render_enabled": False,
        "disabled_event_source_mappings": disabled_mappings,
        "throttled_functions": throttled,
        "stopped_executions": stopped,
    }
    log_event("budget_shutdown_complete", **result)
    return result
