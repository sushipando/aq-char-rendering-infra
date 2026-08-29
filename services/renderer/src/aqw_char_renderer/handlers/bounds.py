from __future__ import annotations

from typing import Any

from aqw_char_renderer.config import RuntimeConfig
from aqw_char_renderer.jobs import JobStore
from aqw_char_renderer.stages.bounds import reduce_bounds
from aqw_char_renderer.storage import S3ObjectStore
from aqw_char_renderer.structured_logging import log_event


def handler(event: dict[str, Any], _context: Any) -> dict[str, Any]:
    config = RuntimeConfig.from_env()
    JobStore(config.job_table).update_status(event["job_id"], "REDUCING_BOUNDS")
    result = reduce_bounds(
        job_id=event["job_id"],
        manifest_key=event["manifest_key"],
        compose_results=event["compose_results"],
        store=S3ObjectStore(),
        config=config,
    )
    log_event("bounds_reduced", job_id=event["job_id"])
    return result
