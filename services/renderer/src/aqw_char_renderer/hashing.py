"""Canonical content hashing for immutable render cache keys."""

from __future__ import annotations

import hashlib
import json
from pathlib import Path
from typing import Any


def canonical_json(value: Any) -> bytes:
    return json.dumps(
        value,
        ensure_ascii=False,
        allow_nan=False,
        separators=(",", ":"),
        sort_keys=True,
    ).encode("utf-8")


def canonical_sha256(value: Any) -> str:
    return hashlib.sha256(canonical_json(value)).hexdigest()


def file_sha256(path: Path, chunk_size: int = 1024 * 1024) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        while chunk := source.read(chunk_size):
            digest.update(chunk)
    return digest.hexdigest()


def render_key(renderer_version: str, quality: float, output_size: int, digest: str) -> str:
    quality_name = f"q{quality:g}".replace(".", "_")
    return f"renders/{renderer_version}/{quality_name}/{output_size}/{digest[:2]}/{digest}.webp"
