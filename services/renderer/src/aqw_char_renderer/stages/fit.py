"""Union per-frame tight visible bounds into one fitted animation canvas."""

from __future__ import annotations

from typing import Any, Protocol

from aqw_char_renderer import character_svg
from aqw_char_renderer.config import RuntimeConfig
from aqw_char_renderer.geometry import shared_canvas


class StageStore(Protocol):
    def read_json(self, bucket: str, key: str) -> Any: ...
    def write_json(self, bucket: str, key: str, value: Any) -> None: ...


def fit_canvas(
    *,
    job_id: str,
    manifest_key: str,
    probe_results: list[dict[str, Any]],
    store: StageStore,
    config: RuntimeConfig,
) -> dict[str, Any]:
    """Union every frame's probed tight bounds into the shared viewBox."""
    prepared = store.read_json(config.work_bucket, manifest_key)
    if prepared.get("job_id") != job_id:
        raise character_svg.CharacterSvgError("Prepare manifest belongs to another job")
    frame_count = int(prepared["frame_count"])
    bounds_by_frame: dict[int, tuple[float, float, float, float]] = {}
    for result in probe_results:
        batch = store.read_json(config.work_bucket, result["batch_manifest_key"])
        if batch.get("job_id") != job_id:
            raise character_svg.CharacterSvgError(
                "Probe batch manifest belongs to another job"
            )
        for frame in batch["frames"]:
            number = int(frame["frame"])
            values = tuple(float(value) for value in frame["bounds"])
            if number in bounds_by_frame:
                raise character_svg.CharacterSvgError(
                    f"Duplicate probed frame {number}"
                )
            bounds_by_frame[number] = values
    expected = set(range(1, frame_count + 1))
    if set(bounds_by_frame) != expected:
        missing = sorted(expected.difference(bounds_by_frame))
        raise character_svg.CharacterSvgError(
            f"Probed frame set is incomplete; missing={missing[:10]}"
        )
    max_size = int(prepared["settings"]["max_size"])
    padding = int(prepared["settings"]["padding"])
    viewbox = shared_canvas(
        (bounds_by_frame[number] for number in sorted(bounds_by_frame)),
        max_size=max_size,
        padding=padding,
    )
    output_key = f"jobs/{job_id}/fitted-canvas.json"
    store.write_json(
        config.work_bucket,
        output_key,
        {
            "schema_version": 1,
            "job_id": job_id,
            "viewbox": list(viewbox),
            "max_size": max_size,
            "padding": padding,
            "frame_count": frame_count,
        },
    )
    return {"job_id": job_id, "fitted_canvas_key": output_key}