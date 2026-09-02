from __future__ import annotations

from time import perf_counter
from typing import Any

from aqw_char_renderer.config import RuntimeConfig
from aqw_char_renderer.stages.component_raster import (
    component_workflow_result,
    rasterize_component_state,
)
from aqw_char_renderer.storage import S3ObjectStore
from aqw_char_renderer.structured_logging import log_event


def handler(event: dict[str, Any], _context: Any) -> dict[str, Any]:
    started = perf_counter()
    config = RuntimeConfig.from_env()
    result = rasterize_component_state(
        job_id=event["job_id"],
        manifest_key=event["manifest_key"],
        task_index=int(event["task_index"]),
        store=S3ObjectStore(),
        config=config,
    )
    log_event(
        "component_raster_complete",
        job_id=event["job_id"],
        task_id=result["task_id"],
        empty=result.get("empty", False),
        png_key=result.get("png_key"),
        duration_ms=round((perf_counter() - started) * 1000),
    )
    return component_workflow_result(result)
