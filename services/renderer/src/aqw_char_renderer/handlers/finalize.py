from __future__ import annotations

from time import perf_counter
from typing import Any

from aqw_char_renderer.config import RuntimeConfig
from aqw_char_renderer.jobs import JobStore
from aqw_char_renderer.stages.finalize import finalize_job
from aqw_char_renderer.storage import S3ObjectStore
from aqw_char_renderer.structured_logging import log_event


def handler(event: dict[str, Any], _context: Any) -> dict[str, Any]:
    started = perf_counter()
    config = RuntimeConfig.from_env()
    jobs = JobStore(config.job_table)
    jobs.update_status(event["job_id"], "FINALIZING")
    result = finalize_job(
        job_id=event["job_id"],
        manifest_key=event["manifest_key"],
        raster_results=event["raster_results"],
        store=S3ObjectStore(),
        config=config,
    )
    log_event(
        "finalize_complete",
        job_id=event["job_id"],
        render_hash=result["render_hash"],
        cache_hit=result["cache_hit"],
        frame_count=result["frame_count"],
        output_bytes=result["bytes"],
        duration_ms=round((perf_counter() - started) * 1000),
    )
    return result
