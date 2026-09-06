"""Atomic DynamoDB job admission, terminal transitions, and slot release."""

from __future__ import annotations

from collections.abc import Mapping
from datetime import UTC, datetime, timedelta
from decimal import Decimal
from typing import Any

import boto3
from boto3.dynamodb.types import TypeDeserializer, TypeSerializer
from botocore.exceptions import ClientError

from aqw_char_renderer.contracts import JobRequest, utc_now

_SERIALIZER = TypeSerializer()
_DESERIALIZER = TypeDeserializer()
TERMINAL_STATUSES = frozenset({"CACHE_HIT", "SUCCEEDED", "FAILED", "TIMED_OUT", "ABORTED"})


def discord_notification_pending(record: Mapping[str, Any]) -> bool:
    """Only explicit Discord admissions may publish to the bot result queue."""
    return (
        record.get("request_origin") == "discord"
        and record.get("result_payload") is not None
        and record.get("result_enqueued_at") is None
    )


def _dynamodb_value(value: Any) -> Any:
    if isinstance(value, float):
        return Decimal(str(value))
    if isinstance(value, Mapping):
        return {key: _dynamodb_value(component) for key, component in value.items()}
    if isinstance(value, list):
        return [_dynamodb_value(component) for component in value]
    if isinstance(value, tuple):
        return [_dynamodb_value(component) for component in value]
    return value


def _values(values: Mapping[str, Any]) -> dict[str, Any]:
    return {key: _SERIALIZER.serialize(_dynamodb_value(value)) for key, value in values.items()}


def _item(values: Mapping[str, Any]) -> dict[str, Any]:
    return {key: _SERIALIZER.serialize(_dynamodb_value(value)) for key, value in values.items()}


class UserLimitExceeded(RuntimeError):
    pass


class JobStore:
    def __init__(self, table_name: str, client: Any | None = None) -> None:
        self.table_name = table_name
        self.client = client or boto3.client("dynamodb")

    def acquire(self, request: JobRequest, maximum_active: int) -> None:
        """Admit an operator/CLI job, including smoke tests and full restarts.

        Discord interactions use the bot's own admission transaction. Keep
        origin outside the render request so copying a Discord job's inputs
        does not also copy permission to send notifications.
        """
        now = utc_now()
        expires_at = int((datetime.now(UTC) + timedelta(days=35)).timestamp())
        job = {
            "PK": f"JOB#{request.job_id}",
            "SK": "META",
            "job_id": request.job_id,
            "user_id": request.discord.user_id,
            "guild_id": request.discord.guild_id or "",
            "channel_id": request.discord.channel_id,
            "request_origin": "cli",
            "status": "QUEUED",
            "slot_released": False,
            "created_at": request.created_at,
            "updated_at": now,
            "expires_at": expires_at,
            "request": request.to_dict(),
        }
        try:
            self.client.transact_write_items(
                TransactItems=[
                    {
                        "Put": {
                            "TableName": self.table_name,
                            "Item": _item(job),
                            "ConditionExpression": "attribute_not_exists(PK)",
                        }
                    },
                    {
                        "Update": {
                            "TableName": self.table_name,
                            "Key": _item(
                                {"PK": f"USER#{request.discord.user_id}", "SK": "COUNTER"}
                            ),
                            "UpdateExpression": (
                                "SET active_count = if_not_exists(active_count, :zero) + :one, "
                                "updated_at = :now"
                            ),
                            "ConditionExpression": (
                                "attribute_not_exists(active_count) OR active_count < :limit"
                            ),
                            "ExpressionAttributeValues": _values(
                                {":zero": 0, ":one": 1, ":limit": maximum_active, ":now": now}
                            ),
                        }
                    },
                ]
            )
        except ClientError as error:
            if error.response.get("Error", {}).get("Code") == "TransactionCanceledException":
                raise UserLimitExceeded(
                    f"User already has {maximum_active} active character jobs"
                ) from error
            raise

    def get(self, job_id: str, *, consistent: bool = True) -> dict[str, Any] | None:
        response = self.client.get_item(
            TableName=self.table_name,
            Key=_item({"PK": f"JOB#{job_id}", "SK": "META"}),
            ConsistentRead=consistent,
        )
        raw = response.get("Item")
        if raw is None:
            return None
        return {key: _DESERIALIZER.deserialize(value) for key, value in raw.items()}

    def mark_execution(self, job_id: str, execution_arn: str) -> None:
        self.client.update_item(
            TableName=self.table_name,
            Key=_item({"PK": f"JOB#{job_id}", "SK": "META"}),
            UpdateExpression="SET execution_arn = :arn, #status = :status, updated_at = :now",
            ConditionExpression="attribute_exists(PK)",
            ExpressionAttributeNames={"#status": "status"},
            ExpressionAttributeValues=_values(
                {":arn": execution_arn, ":status": "PREPARING", ":now": utc_now()}
            ),
        )

    def update_status(self, job_id: str, status: str, **attributes: Any) -> None:
        names = {"#status": "status"}
        values: dict[str, Any] = {":status": status, ":now": utc_now()}
        setters = ["#status = :status", "updated_at = :now"]
        for index, (name, value) in enumerate(sorted(attributes.items())):
            name_token = f"#field{index}"
            value_token = f":value{index}"
            names[name_token] = name
            values[value_token] = value
            setters.append(f"{name_token} = {value_token}")
        self.client.update_item(
            TableName=self.table_name,
            Key=_item({"PK": f"JOB#{job_id}", "SK": "META"}),
            UpdateExpression="SET " + ", ".join(setters),
            ExpressionAttributeNames=names,
            ExpressionAttributeValues=_values(values),
        )

    def release(
        self,
        job_id: str,
        status: str,
        *,
        attributes: Mapping[str, Any] | None = None,
    ) -> bool:
        if status not in TERMINAL_STATUSES:
            raise ValueError(f"Cannot release a nonterminal status: {status}")
        current = self.get(job_id)
        if current is None:
            return False
        if current.get("slot_released") is True:
            return False
        user_id = str(current["user_id"])
        names = {"#status": "status"}
        job_values: dict[str, Any] = {
            ":status": status,
            ":true": True,
            ":false": False,
            ":now": utc_now(),
        }
        setters = [
            "#status = :status",
            "slot_released = :true",
            "updated_at = :now",
        ]
        for index, (name, value) in enumerate(sorted((attributes or {}).items())):
            name_token = f"#field{index}"
            value_token = f":value{index}"
            names[name_token] = name
            job_values[value_token] = value
            setters.append(f"{name_token} = {value_token}")
        try:
            self.client.transact_write_items(
                TransactItems=[
                    {
                        "Update": {
                            "TableName": self.table_name,
                            "Key": _item({"PK": f"JOB#{job_id}", "SK": "META"}),
                            "UpdateExpression": "SET " + ", ".join(setters),
                            "ConditionExpression": (
                                "attribute_exists(PK) AND "
                                "(attribute_not_exists(slot_released) OR slot_released = :false)"
                            ),
                            "ExpressionAttributeNames": names,
                            "ExpressionAttributeValues": _values(job_values),
                        }
                    },
                    {
                        "Update": {
                            "TableName": self.table_name,
                            "Key": _item({"PK": f"USER#{user_id}", "SK": "COUNTER"}),
                            "UpdateExpression": "SET active_count = active_count - :one, updated_at = :now",
                            "ConditionExpression": "active_count > :zero",
                            "ExpressionAttributeValues": _values(
                                {":one": 1, ":zero": 0, ":now": job_values[":now"]}
                            ),
                        }
                    },
                ]
            )
        except ClientError as error:
            if error.response.get("Error", {}).get("Code") == "TransactionCanceledException":
                reread = self.get(job_id)
                if reread is not None and reread.get("slot_released") is True:
                    return False
            raise
        return True

    def mark_result_enqueued(self, job_id: str) -> None:
        self.update_status(job_id, str(self.get(job_id)["status"]), result_enqueued_at=utc_now())

    def scan_reconcilable(self, *, maximum: int = 200) -> list[dict[str, Any]]:
        """Return a bounded set of job records that may need repair."""
        records: list[dict[str, Any]] = []
        start_key: dict[str, Any] | None = None
        while len(records) < maximum:
            arguments: dict[str, Any] = {
                "TableName": self.table_name,
                "FilterExpression": "begins_with(PK, :prefix)",
                "ExpressionAttributeValues": _values({":prefix": "JOB#"}),
                "Limit": min(100, maximum - len(records)),
            }
            if start_key is not None:
                arguments["ExclusiveStartKey"] = start_key
            response = self.client.scan(**arguments)
            records.extend(
                {key: _DESERIALIZER.deserialize(value) for key, value in item.items()}
                for item in response.get("Items", [])
            )
            start_key = response.get("LastEvaluatedKey")
            if not start_key:
                break
        return records[:maximum]
