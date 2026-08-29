"""Apply the shared canvas, rasterize SVGs, and encode delta WebP frames."""

from __future__ import annotations

import subprocess
import tempfile
import xml.etree.ElementTree as ET
from pathlib import Path
from typing import Any, Protocol

from PIL import Image

from aqw_char_renderer import character_svg
from aqw_char_renderer.config import RuntimeConfig
from aqw_char_renderer.hashing import file_sha256


class StageStore(Protocol):
    def download(self, bucket: str, key: str, destination: Path, **kwargs: Any) -> Path: ...
    def upload_file(self, source: Path, bucket: str, key: str, **kwargs: Any) -> None: ...
    def read_json(self, bucket: str, key: str) -> Any: ...
    def write_json(self, bucket: str, key: str, value: Any) -> None: ...


def _rasterize(
    source: Path,
    destination: Path,
    *,
    viewbox: tuple[float, float, float, float],
    max_size: int,
    rsvg_convert: str,
) -> tuple[int, int]:
    try:
        tree = ET.parse(source)
    except (OSError, ET.ParseError) as error:
        raise character_svg.CharacterSvgError(f"Invalid composed SVG {source}: {error}") from error
    character_svg._set_output_geometry(tree.getroot(), viewbox, max_size=max_size)
    character_svg.calibrate_minimum_strokes(tree.getroot())
    aligned = destination.with_suffix(".svg")
    aligned.parent.mkdir(parents=True, exist_ok=True)
    tree.write(aligned, encoding="utf-8", xml_declaration=True)
    character_svg.render_preview(aligned, destination, max_size=max_size, rsvg_convert=rsvg_convert)
    with Image.open(destination) as image:
        if image.mode != "RGBA":
            raise character_svg.CharacterSvgError("Raster frame is not transparent RGBA")
        return image.size


def _encode_frame(
    current: Path,
    previous: Path | None,
    output: Path,
    *,
    quality: float,
    method: int,
    cwebp: str,
) -> tuple[int, int, int, int, tuple[int, int]]:
    x, y, width, height, canvas = character_svg.animation_delta_crop(current, previous)
    command = [
        cwebp,
        "-quiet",
        "-q",
        f"{quality:g}",
        "-alpha_q",
        "100",
        "-m",
        str(method),
    ]
    if (x, y, width, height) != (0, 0, canvas[0], canvas[1]):
        command.extend(("-crop", str(x), str(y), str(width), str(height)))
    command.extend((str(current), "-o", str(output)))
    result = subprocess.run(command, capture_output=True, text=True, check=False)
    if result.returncode:
        detail = (result.stderr or result.stdout).strip()
        raise character_svg.CharacterSvgError(f"cwebp failed: {detail[-2000:]}")
    if not output.is_file() or output.stat().st_size == 0:
        raise character_svg.CharacterSvgError("cwebp produced an empty frame")
    return x, y, width, height, canvas


def raster_batch(
    *,
    job_id: str,
    manifest_key: str,
    shared_canvas_key: str,
    batch: dict[str, int],
    store: StageStore,
    config: RuntimeConfig,
) -> dict[str, Any]:
    prepared = store.read_json(config.work_bucket, manifest_key)
    shared = store.read_json(config.work_bucket, shared_canvas_key)
    if prepared.get("job_id") != job_id or shared.get("job_id") != job_id:
        raise character_svg.CharacterSvgError("Raster input belongs to another job")
    batch_index = int(batch["index"])
    frame_start = int(batch["frame_start"])
    frame_end = int(batch["frame_end"])
    viewbox = tuple(float(value) for value in shared["viewbox"])
    max_size = int(prepared["settings"]["max_size"])
    durations = [int(value) for value in prepared["frame_durations"]]
    records: list[dict[str, Any]] = []

    with tempfile.TemporaryDirectory(prefix=f"aqw-raster-{job_id}-{batch_index}-") as temporary:
        root = Path(temporary)
        pngs: dict[int, Path] = {}
        first_needed = frame_start - 1 if frame_start > 1 else frame_start
        for frame_number in range(first_needed, frame_end + 1):
            svg_key = f"jobs/{job_id}/svg/{frame_number:06d}.svg"
            svg = store.download(
                config.work_bucket,
                svg_key,
                root / "input" / f"{frame_number:06d}.svg",
            )
            png = root / "png" / f"{frame_number:06d}.png"
            _rasterize(
                svg,
                png,
                viewbox=viewbox,  # type: ignore[arg-type]
                max_size=max_size,
                rsvg_convert=config.rsvg_convert,
            )
            pngs[frame_number] = png

        canvas_size: tuple[int, int] | None = None
        for frame_number in range(frame_start, frame_end + 1):
            encoded = root / "webp" / f"{frame_number:06d}.webp"
            encoded.parent.mkdir(parents=True, exist_ok=True)
            crop = _encode_frame(
                pngs[frame_number],
                pngs.get(frame_number - 1),
                encoded,
                quality=float(prepared["settings"]["webp_quality"]),
                method=int(prepared["settings"]["webp_method"]),
                cwebp=config.cwebp,
            )
            x, y, width, height, frame_canvas = crop
            if canvas_size is None:
                canvas_size = frame_canvas
            elif frame_canvas != canvas_size:
                raise character_svg.CharacterSvgError("Raster frames do not share one canvas")
            output_key = f"jobs/{job_id}/webp-frames/{frame_number:06d}.webp"
            store.upload_file(encoded, config.work_bucket, output_key, content_type="image/webp")
            records.append(
                {
                    "frame": frame_number,
                    "webp_key": output_key,
                    "x": x,
                    "y": y,
                    "width": width,
                    "height": height,
                    "canvas_width": frame_canvas[0],
                    "canvas_height": frame_canvas[1],
                    "duration": durations[frame_number - 1],
                    "sha256": file_sha256(encoded),
                    "bytes": encoded.stat().st_size,
                }
            )

    batch_manifest_key = f"jobs/{job_id}/encode/batch-{batch_index:04d}.json"
    store.write_json(
        config.work_bucket,
        batch_manifest_key,
        {
            "schema_version": 1,
            "job_id": job_id,
            "batch": batch_index,
            "frames": records,
        },
    )
    return {
        "job_id": job_id,
        "batch": batch_index,
        "batch_manifest_key": batch_manifest_key,
    }
