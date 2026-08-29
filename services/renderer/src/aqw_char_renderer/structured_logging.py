"""Small structured logger that keeps secrets and large payloads out of logs."""

from __future__ import annotations

import json
import logging
from typing import Any

LOGGER = logging.getLogger("aqw_char_renderer")
LOGGER.setLevel(logging.INFO)


def log_event(event: str, **fields: Any) -> None:
    payload = {"event": event, **{key: value for key, value in fields.items() if value is not None}}
    LOGGER.info(json.dumps(payload, default=str, separators=(",", ":"), sort_keys=True))
