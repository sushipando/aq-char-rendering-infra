from __future__ import annotations

from time import perf_counter
from typing import Any

from aqw_char_renderer.config import RuntimeConfig
from aqw_char_renderer.contracts import JobRequest
from aqw_char_renderer.handlers.complete import complete_success
from aqw_char_renderer.jobs import JobStore
from aqw_char_renderer.stages.compose_frames import compose_all_frames
from aqw_char_renderer.storage import S3ObjectStore
from aqw_char_renderer.structured_logging import log_event


def handler(event: dict[str, Any], _context: Any) -> dict[str, Any]:
    started = perf_counter()
    config = RuntimeConfig.from_env()
    jobs = JobStore(config.job_table)
    jobs.update_status(event["job_id"], "FINALIZING")
    result = compose_all_frames(
        job_id=event["job_id"],
        manifest_key=event["manifest_key"],
        component_results=event["component_results"],
        store=S3ObjectStore(
            max_pool_connections=config.finalizer_download_concurrency,
        ),
        config=config,
    )
    # Complete the job inline, exactly like the legacy finalize handler.
    completion = complete_success(
        config=config,
        jobs=jobs,
        request=JobRequest.from_dict(event["request"]),
        result=result,
    )
    log_event(
        "compose_all_complete",
        job_id=event["job_id"],
        render_hash=result["render_hash"],
        frame_count=result["frame_count"],
        output_bytes=result["bytes"],
        released=completion["released"],
        duration_ms=round((perf_counter() - started) * 1000),
    )
    return result