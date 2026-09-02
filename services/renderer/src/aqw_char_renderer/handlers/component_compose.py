from __future__ import annotations

import time
from time import perf_counter
from typing import Any

from aqw_char_renderer.config import RuntimeConfig
from aqw_char_renderer.stages.component_compose import compose_frame_batch
from aqw_char_renderer.storage import S3ObjectStore
from aqw_char_renderer.structured_logging import log_event

_MODULE_LOADED_AT = time.perf_counter()
_COLD_START = True


def handler(event: dict[str, Any], _context: Any) -> dict[str, Any]:
    global _COLD_START
    started = perf_counter()
    cold_start = _COLD_START
    _COLD_START = False
    config = RuntimeConfig.from_env()
    result = compose_frame_batch(
        job_id=event["job_id"],
        manifest_key=event["manifest_key"],
        component_results=event["component_results"],
        batch=event["batch"],
        store=S3ObjectStore(
            max_pool_connections=config.finalizer_download_concurrency,
        ),
        config=config,
        compositor=event.get("compositor"),
    )
    log_event(
        "component_compose_complete",
        job_id=event["job_id"],
        batch=result["batch"],
        frame_start=event["batch"]["frame_start"],
        frame_end=event["batch"]["frame_end"],
        compositor=event.get("compositor") or config.component_compositor,
        cold_start=cold_start,
        module_age_ms=round((started - _MODULE_LOADED_AT) * 1000),
        duration_ms=round((perf_counter() - started) * 1000),
    )
    return result
