from __future__ import annotations

from time import perf_counter
from typing import Any

from aqw_char_renderer.config import RuntimeConfig
from aqw_char_renderer.contracts import JobRequest
from aqw_char_renderer.jobs import JobStore
from aqw_char_renderer.stages.prepare import (
    prepare_export_source,
    prepare_finish,
    prepare_resolve,
)
from aqw_char_renderer.storage import S3ObjectStore
from aqw_char_renderer.structured_logging import log_event


def handler(event: dict[str, Any], _context: Any) -> dict[str, Any]:
    started = perf_counter()
    config = RuntimeConfig.from_env()
    store = S3ObjectStore()
    phase = event.get("phase", "resolve")
    if phase == "export":
        result = prepare_export_source(
            job_id=event["job_id"],
            input_key=event["input_key"],
            source=event["source"],
            store=store,
            config=config,
        )
        return result
    request = JobRequest.from_dict(event["request"])
    jobs = JobStore(config.job_table)
    jobs.update_status(request.job_id, "PREPARING")
    if phase == "finish":
        result = prepare_finish(
            request=request,
            input_key=event["input_key"],
            export_results=event["export_results"],
            store=store,
            config=config,
        )
        log_event(
            "prepare_complete",
            job_id=request.job_id,
            discord_user_id=request.discord.user_id,
            cache_hit=False,
            frame_count=result.get("frame_count"),
            duration_ms=round((perf_counter() - started) * 1000),
        )
        return result
    result = prepare_resolve(
        request,
        store=store,
        config=config,
        flashvars=request.appearance,
    )
    log_event(
        "prepare_complete",
        job_id=request.job_id,
        discord_user_id=request.discord.user_id,
        cache_hit=result["cache_hit"],
        frame_count=result.get("frame_count"),
        duration_ms=round((perf_counter() - started) * 1000),
    )
    return result