"""Reduce per-frame visible bounds to one animation canvas."""

from __future__ import annotations

from typing import Any, Protocol

from aqw_char_renderer.config import RuntimeConfig
from aqw_char_renderer.geometry import shared_canvas


class StageStore(Protocol):
    def read_json(self, bucket: str, key: str) -> Any: ...
    def write_json(self, bucket: str, key: str, value: Any) -> None: ...


def reduce_bounds(
    *,
    job_id: str,
    manifest_key: str,
    compose_results: list[dict[str, Any]],
    store: StageStore,
    config: RuntimeConfig,
) -> dict[str, Any]:
    prepared = store.read_json(config.work_bucket, manifest_key)
    expected_count = int(prepared["frame_count"])
    frame_records: dict[int, dict[str, Any]] = {}
    for result in compose_results:
        batch = store.read_json(config.work_bucket, result["batch_manifest_key"])
        if batch.get("job_id") != job_id:
            raise ValueError("Compose batch manifest belongs to another job")
        for frame in batch["frames"]:
            number = int(frame["frame"])
            if number in frame_records:
                raise ValueError(f"Duplicate composed frame {number}")
            frame_records[number] = frame
    expected = set(range(1, expected_count + 1))
    if set(frame_records) != expected:
        missing = sorted(expected.difference(frame_records))
        raise ValueError(f"Composed frame set is incomplete; missing={missing[:10]}")
    canvas = shared_canvas(
        (frame_records[index]["bounds"] for index in sorted(frame_records)),
        max_size=int(prepared["settings"]["max_size"]),
        padding=int(prepared["settings"]["padding"]),
    )
    output_key = f"jobs/{job_id}/shared-canvas.json"
    payload = {
        "schema_version": 1,
        "job_id": job_id,
        "viewbox": list(canvas),
        "max_size": int(prepared["settings"]["max_size"]),
        "padding": int(prepared["settings"]["padding"]),
        "frame_count": expected_count,
    }
    store.write_json(config.work_bucket, output_key, payload)
    return {"job_id": job_id, "shared_canvas_key": output_key}
