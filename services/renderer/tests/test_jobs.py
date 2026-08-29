from __future__ import annotations

from typing import Any

from boto3.dynamodb.types import TypeSerializer
from test_contracts_and_core import request_payload

from aqw_char_renderer.contracts import JobRequest
from aqw_char_renderer.jobs import JobStore

SERIALIZER = TypeSerializer()


def item(value: dict[str, Any]) -> dict[str, Any]:
    return {key: SERIALIZER.serialize(component) for key, component in value.items()}


class FakeDynamo:
    def __init__(self, record: dict[str, Any] | None = None) -> None:
        self.record = record
        self.transactions: list[list[dict[str, Any]]] = []
        self.updates: list[dict[str, Any]] = []

    def transact_write_items(self, *, TransactItems: list[dict[str, Any]]) -> None:
        self.transactions.append(TransactItems)

    def get_item(self, **_kwargs: Any) -> dict[str, Any]:
        return {} if self.record is None else {"Item": item(self.record)}

    def update_item(self, **kwargs: Any) -> None:
        self.updates.append(kwargs)


def test_acquire_is_one_transaction_for_unique_job_and_user_counter() -> None:
    client = FakeDynamo()
    store = JobStore("jobs", client=client)
    request = JobRequest.from_dict(request_payload())
    store.acquire(request, maximum_active=2)
    transaction = client.transactions[0]
    assert len(transaction) == 2
    assert transaction[0]["Put"]["ConditionExpression"] == "attribute_not_exists(PK)"
    assert "active_count < :limit" in transaction[1]["Update"]["ConditionExpression"]


def test_release_is_atomic_and_does_not_send_unused_expression_values() -> None:
    client = FakeDynamo(
        {
            "PK": "JOB#8d1c70fd-6c7a-4abc-a539-014575b09078",
            "SK": "META",
            "job_id": "8d1c70fd-6c7a-4abc-a539-014575b09078",
            "user_id": "123456789012345678",
            "slot_released": False,
        }
    )
    store = JobStore("jobs", client=client)
    assert store.release(
        "8d1c70fd-6c7a-4abc-a539-014575b09078",
        "SUCCEEDED",
        attributes={"result_url": "https://example.com/result.webp"},
    )
    job_update = client.transactions[0][0]["Update"]
    counter_update = client.transactions[0][1]["Update"]
    assert ":one" not in job_update["ExpressionAttributeValues"]
    assert set(counter_update["ExpressionAttributeValues"]) == {":one", ":zero", ":now"}


def test_release_is_idempotent_when_job_is_already_released() -> None:
    client = FakeDynamo(
        {
            "PK": "JOB#8d1c70fd-6c7a-4abc-a539-014575b09078",
            "SK": "META",
            "job_id": "8d1c70fd-6c7a-4abc-a539-014575b09078",
            "user_id": "123456789012345678",
            "slot_released": True,
        }
    )
    store = JobStore("jobs", client=client)
    assert not store.release("8d1c70fd-6c7a-4abc-a539-014575b09078", "FAILED")
    assert client.transactions == []
