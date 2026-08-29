"""Validate encoded frames, mux one animation, and atomically publish it."""

from __future__ import annotations

import subprocess
import tempfile
from collections.abc import Mapping
from pathlib import Path
from typing import Any, Protocol

from PIL import Image

from aqw_char_renderer import character_svg
from aqw_char_renderer.config import RuntimeConfig
from aqw_char_renderer.hashing import file_sha256


class StageStore(Protocol):
    def exists(self, bucket: str, key: str) -> Mapping[str, Any] | None: ...
    def download(
        self,
        bucket: str,
        key: str,
        destination: Path,
        *,
        expected_sha256: str | None = None,
    ) -> Path: ...
    def upload_file(self, source: Path, bucket: str, key: str, **kwargs: Any) -> None: ...
    def copy(self, bucket: str, source_key: str, destination_key: str, **kwargs: Any) -> None: ...
    def delete(self, bucket: str, key: str) -> None: ...
    def read_json(self, bucket: str, key: str) -> Any: ...


def _ordered_frames(
    job_id: str,
    frame_count: int,
    raster_results: list[dict[str, Any]],
    *,
    store: StageStore,
    config: RuntimeConfig,
) -> list[dict[str, Any]]:
    by_number: dict[int, dict[str, Any]] = {}
    for result in raster_results:
        batch = store.read_json(config.work_bucket, result["batch_manifest_key"])
        if batch.get("job_id") != job_id:
            raise character_svg.CharacterSvgError("Encode manifest belongs to another job")
        for frame in batch["frames"]:
            number = int(frame["frame"])
            if number in by_number:
                raise character_svg.CharacterSvgError(f"Duplicate encoded frame {number}")
            by_number[number] = frame
    expected = list(range(1, frame_count + 1))
    if sorted(by_number) != expected:
        raise character_svg.CharacterSvgError("Encoded frame set is incomplete")
    frames = [by_number[number] for number in expected]
    canvases = {(int(frame["canvas_width"]), int(frame["canvas_height"])) for frame in frames}
    if len(canvases) != 1:
        raise character_svg.CharacterSvgError("Encoded frames do not share one canvas")
    return frames


def _validate_animation(
    path: Path,
    *,
    frame_count: int,
    canvas: tuple[int, int],
) -> None:
    try:
        with Image.open(path) as image:
            if image.format != "WEBP":
                raise character_svg.CharacterSvgError("Final output is not WebP")
            if getattr(image, "n_frames", 1) != frame_count:
                raise character_svg.CharacterSvgError("Final WebP frame count is incorrect")
            if image.size != canvas:
                raise character_svg.CharacterSvgError("Final WebP canvas is incorrect")
            if image.info.get("loop") != 0 and frame_count > 1:
                raise character_svg.CharacterSvgError("Final WebP is not configured to loop")
            image.seek(0)
            if "A" not in image.convert("RGBA").getbands():
                raise character_svg.CharacterSvgError("Final WebP has no alpha channel")
    except OSError as error:
        raise character_svg.CharacterSvgError(f"Cannot validate final WebP: {error}") from error


def finalize_job(
    *,
    job_id: str,
    manifest_key: str,
    raster_results: list[dict[str, Any]],
    store: StageStore,
    config: RuntimeConfig,
) -> dict[str, Any]:
    prepared = store.read_json(config.work_bucket, manifest_key)
    if prepared.get("job_id") != job_id:
        raise character_svg.CharacterSvgError("Prepare manifest belongs to another job")
    frame_count = int(prepared["frame_count"])
    frames = _ordered_frames(job_id, frame_count, raster_results, store=store, config=config)
    canvas = (int(frames[0]["canvas_width"]), int(frames[0]["canvas_height"]))
    final_key = str(prepared["final_key"])
    cached = store.exists(config.work_bucket, final_key)
    if cached is not None:
        return {
            "url": f"{config.public_base_url}/{final_key}",
            "frame_count": frame_count,
            "width": canvas[0],
            "height": canvas[1],
            "duration_ms": sum(int(frame["duration"]) for frame in frames),
            "bytes": int(cached.get("ContentLength", 0)),
            "cache_hit": True,
            "render_hash": prepared["render_hash"],
            "final_key": final_key,
        }

    with tempfile.TemporaryDirectory(prefix=f"aqw-finalize-{job_id}-") as temporary:
        root = Path(temporary)
        local_frames: list[tuple[Path, dict[str, Any]]] = []
        for frame in frames:
            path = store.download(
                config.work_bucket,
                frame["webp_key"],
                root / "frames" / f"{int(frame['frame']):06d}.webp",
                expected_sha256=frame["sha256"],
            )
            local_frames.append((path, frame))
        output = root / "result.webp"
        command = [config.webpmux]
        for path, frame in local_frames:
            command.extend(
                (
                    "-frame",
                    str(path),
                    f"+{int(frame['duration'])}+{int(frame['x'])}+{int(frame['y'])}+0-b",
                )
            )
        command.extend(("-loop", "0", "-bgcolor", "0,0,0,0", "-o", str(output)))
        result = subprocess.run(command, capture_output=True, text=True, check=False)
        if result.returncode:
            detail = (result.stderr or result.stdout).strip()
            raise character_svg.CharacterSvgError(f"webpmux failed: {detail[-2000:]}")
        _validate_animation(output, frame_count=frame_count, canvas=canvas)
        duration_ms = sum(int(frame["duration"]) for frame in frames)
        temporary_key = f"jobs/{job_id}/final/result.webp"
        metadata = {
            "render-hash": str(prepared["render_hash"]),
            "frame-count": str(frame_count),
            "width": str(canvas[0]),
            "height": str(canvas[1]),
            "duration-ms": str(duration_ms),
            "sha256": file_sha256(output),
        }
        store.upload_file(
            output,
            config.work_bucket,
            temporary_key,
            content_type="image/webp",
            metadata=metadata,
        )
        try:
            store.copy(
                config.work_bucket,
                temporary_key,
                final_key,
                content_type="image/webp",
                cache_control="public, max-age=86400",
                metadata=metadata,
            )
        finally:
            store.delete(config.work_bucket, temporary_key)
        return {
            "url": f"{config.public_base_url}/{final_key}",
            "frame_count": frame_count,
            "width": canvas[0],
            "height": canvas[1],
            "duration_ms": duration_ms,
            "bytes": output.stat().st_size,
            "cache_hit": False,
            "render_hash": prepared["render_hash"],
            "final_key": final_key,
        }
