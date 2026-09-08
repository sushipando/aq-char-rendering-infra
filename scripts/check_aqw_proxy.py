#!/usr/bin/env python3
"""Live AQW proxy check; no AWS writes or job submission."""
from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import sys
from uuid import uuid4

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "services/renderer/src"))
from aqw_char_renderer.source_http import fetch_bytes, source_session  # noqa: E402
from aqw_char_renderer.legacy import preview_aqw_tryon as tryon  # noqa: E402
from urllib.parse import quote  # noqa: E402


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    group = parser.add_mutually_exclusive_group(required=True)
    group.add_argument("--username")
    group.add_argument("--swf", help="Relative gamefiles SWF path")
    parser.add_argument("--timeout", type=float, default=15)
    args = parser.parse_args()
    if "AQW_BRIGHTDATA_CONFIG" not in os.environ:
        parser.error("Set AQW_BRIGHTDATA_CONFIG first; refusing a direct origin test")
    if args.username:
        fields = tryon.fetch_character_flashvars(args.username, timeout=args.timeout)
        print(json.dumps({"name": fields.get("strName"), "bgindex": fields.get("bgindex"),
                          "field_count": len(fields)}))
    else:
        path = args.swf
        if not path.lower().endswith(".swf") or path.startswith("/") or any(
            part in {"", ".", ".."} for part in path.split("/")
        ) or ":" in path or "\\" in path or "?" in path or "#" in path:
            parser.error("--swf must be a relative gamefiles SWF path")
        data = fetch_bytes(tryon.GAMEFILES_URL + quote(path, safe="/"), timeout=args.timeout,
                           maximum_bytes=16 * 1024 * 1024, user_agent=tryon.USER_AGENT)
        if len(data) < 8 or data[:3] not in {b"FWS", b"CWS", b"ZWS"}:
            raise RuntimeError("Origin response is not a SWF")
        print(json.dumps({"path": path, "bytes": len(data), "sha256": hashlib.sha256(data).hexdigest()}))


if __name__ == "__main__":
    with source_session(str(uuid4())):
        main()
