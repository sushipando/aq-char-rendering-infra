from __future__ import annotations

import time
from time import perf_counter
from typing import Any

from aqw_char_renderer.config import RuntimeConfig
from aqw_char_renderer.stages.render import render_batch
from aqw_char_renderer.storage import S3ObjectStore
from aqw_char_renderer.structured_logging import log_event

# Module load happens once per execution environment (cold start) and is
# reused by warm invocations. Captured before the first handler call.
_MODULE_LOADED_AT = time.perf_counter()
_COLD_START = True


def handler(event: dict[str, Any], _context: Any) -> dict[str, Any]:
    global _COLD_START
    handler_started = perf_counter()
    cold_start = _COLD_START
    _COLD_START = False
    config = RuntimeConfig.from_env()
    result = render_batch(
        job_id=event["job_id"],
        manifest_key=event["manifest_key"],
        batch=event["batch"],
        store=S3ObjectStore(),
        config=config,
    )
    log_event(
        "render_batch_complete",
        job_id=event["job_id"],
        batch=result["batch"],
        frame_start=event["batch"]["frame_start"],
        frame_end=event["batch"]["frame_end"],
        cold_start=cold_start,
        # Warm invocations reuse the loaded runtime; cold ones include the
        # module-import + boto client construction cost.
        module_age_ms=round((handler_started - _MODULE_LOADED_AT) * 1000),
        duration_ms=round((perf_counter() - handler_started) * 1000),
    )
    return result
