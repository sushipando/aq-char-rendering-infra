from __future__ import annotations

from time import perf_counter
from typing import Any

from aqw_char_renderer.config import RuntimeConfig
from aqw_char_renderer.stages.raster import raster_batch
from aqw_char_renderer.storage import S3ObjectStore
from aqw_char_renderer.structured_logging import log_event


def handler(event: dict[str, Any], _context: Any) -> dict[str, Any]:
    started = perf_counter()
    config = RuntimeConfig.from_env()
    result = raster_batch(
        job_id=event["job_id"],
        manifest_key=event["manifest_key"],
        shared_canvas_key=event["shared_canvas_key"],
        batch=event["batch"],
        store=S3ObjectStore(),
        config=config,
    )
    log_event(
        "raster_batch_complete",
        job_id=event["job_id"],
        batch=result["batch"],
        frame_start=event["batch"]["frame_start"],
        frame_end=event["batch"]["frame_end"],
        duration_ms=round((perf_counter() - started) * 1000),
    )
    return result
