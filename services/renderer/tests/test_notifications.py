"""Offline coverage of terminal publication, cleanup, and outbox retries."""

from types import SimpleNamespace
from unittest.mock import Mock

import pytest
from test_contracts_and_core import request_payload

from aqw_char_renderer.contracts import JobRequest
from aqw_char_renderer.handlers import cleanup, complete
from aqw_char_renderer.jobs import discord_notification_pending


class FakeJobs:
    def __init__(self, origin):
        request = JobRequest.from_dict(request_payload())
        self.record = {
            "job_id": request.job_id,
            "user_id": request.discord.user_id,
            "channel_id": request.discord.channel_id,
            "guild_id": request.discord.guild_id,
            "request": request.to_dict(),
            "status": "PREPARING",
            "slot_released": False,
        }
        if origin is not None:
            self.record["request_origin"] = origin
        self.releases = 0
        self.enqueued = 0

    def get(self, job_id):
        assert job_id == self.record["job_id"]
        return self.record

    def release(self, job_id, status, *, attributes):
        if self.get(job_id)["slot_released"]:
            return False
        self.record.update(attributes, status=status, slot_released=True)
        self.releases += 1
        return True

    def mark_result_enqueued(self, job_id):
        self.get(job_id)["result_enqueued_at"] = "2026-09-06T00:00:00Z"
        self.enqueued += 1

    def scan_reconcilable(self):
        return [self.record]


@pytest.mark.parametrize("origin", ["discord", "cli", None, "unknown", True])
@pytest.mark.parametrize("path", ["success", "cache_hit", "failure", "TIMED_OUT", "ABORTED"])
def test_every_terminal_path_preserves_results_but_only_notifies_discord(monkeypatch, origin, path):
    jobs = FakeJobs(origin)
    request = JobRequest.from_dict(jobs.record["request"])
    config = SimpleNamespace(result_queue_url="unused")
    publish = Mock()
    monkeypatch.setattr(complete, "_publish", publish)
    monkeypatch.setattr(cleanup, "_publish", publish)
    monkeypatch.setattr(complete, "JobStore", lambda _table: jobs)
    monkeypatch.setattr(complete.RuntimeConfig, "from_env", lambda: SimpleNamespace(job_table="jobs"))
    result = {
        "url": "https://example.test/renders/test.webp",
        "frame_count": 1, "width": 128, "height": 128,
        "duration_ms": 42, "bytes": 100, "cache_hit": path == "cache_hit",
    }
    for _ in range(2):
        if path in {"success", "cache_hit"}:
            complete.complete_success(config=config, jobs=jobs, request=request, result=result)
        elif path == "failure":
            complete.failure_handler({"request": request.to_dict(), "failure": {}}, None)
        else:
            cleanup._release_failure(jobs, config, jobs.record, path)
    assert jobs.releases == 1
    assert jobs.record["slot_released"] is True
    assert jobs.record["result_payload"]["job_id"] == request.job_id
    if path in {"success", "cache_hit"}:
        assert jobs.record["result_url"] == result["url"]
        assert jobs.record["result_payload"]["result"] == result
    else:
        assert jobs.record["result_payload"]["status"] == "FAILED"
    assert publish.call_count == (1 if origin == "discord" else 0)
    assert jobs.enqueued == publish.call_count
    assert not discord_notification_pending(jobs.record)


@pytest.mark.parametrize("origin", ["discord", "cli", None])
def test_scheduled_reconcile_does_not_requeue_suppressed_results(monkeypatch, origin):
    jobs = FakeJobs(origin)
    jobs.record.update(slot_released=True, status="FAILED", result_payload={"status": "FAILED"})
    publish = Mock()
    monkeypatch.setattr(cleanup, "_publish", publish)
    monkeypatch.setattr(cleanup.boto3, "client", Mock())
    result = cleanup._scheduled_reconcile(SimpleNamespace(), jobs)
    assert result["repaired"] == []
    assert result["requeued"] == ([jobs.record["job_id"]] if origin == "discord" else [])
    assert publish.call_count == (1 if origin == "discord" else 0)
    assert cleanup._scheduled_reconcile(SimpleNamespace(), jobs)["requeued"] == []


def test_retry_publishes_winning_terminal_payload_not_losing_completion(monkeypatch):
    jobs = FakeJobs("discord")
    winner = {"status": "FAILED", "error": {"code": "WORKFLOW_ABORTED"}}
    jobs.record.update(slot_released=True, status="ABORTED", result_payload=winner)
    publish = Mock()
    monkeypatch.setattr(cleanup, "_publish", publish)
    assert not cleanup._release_failure(jobs, SimpleNamespace(), jobs.record, "TIMED_OUT")
    assert publish.call_args.args[1] == winner
