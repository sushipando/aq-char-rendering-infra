from __future__ import annotations

from time import perf_counter
from typing import Any

from aqw_char_renderer.config import RuntimeConfig
from aqw_char_renderer.stages.fit import fit_canvas
from aqw_char_renderer.storage import S3ObjectStore
from aqw_char_renderer.structured_logging import log_event


def handler(event: dict[str, Any], _context: Any) -> dict[str, Any]:
    started = perf_counter()
    config = RuntimeConfig.from_env()
    result = fit_canvas(
        job_id=event["job_id"],
        manifest_key=event["manifest_key"],
        probe_results=event["probe_results"],
        store=S3ObjectStore(),
        config=config,
    )
    log_event(
        "fit_canvas_complete",
        job_id=event["job_id"],
        duration_ms=round((perf_counter() - started) * 1000),
    )
    return result