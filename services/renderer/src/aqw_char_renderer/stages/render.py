"""Compose, rasterize, and delta-encode one batch of frames in a single stage.

This merged stage replaces the old compose -> bounds -> raster chain. Prepare
computes one shared animation canvas up front, so each worker can compose a
frame in memory, rasterize it exactly once, and encode the delta WebP without
any intermediate S3 round-trips.
"""

from __future__ import annotations

import subprocess
import tarfile
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


def _extract_archive(archive_path: Path, target: Path) -> None:
    target.mkdir(parents=True, exist_ok=True)
    with tarfile.open(archive_path) as archive:
        archive.extractall(target, filter="data")


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


def render_batch(
    *,
    job_id: str,
    manifest_key: str,
    batch: dict[str, int],
    store: StageStore,
    config: RuntimeConfig,
) -> dict[str, Any]:
    prepared = store.read_json(config.work_bucket, manifest_key)
    if prepared.get("job_id") != job_id:
        raise character_svg.CharacterSvgError("Prepare manifest belongs to another job")
    batch_index = int(batch["index"])
    frame_start = int(batch["frame_start"])
    frame_end = int(batch["frame_end"])
    frame_count = int(prepared["frame_count"])
    if frame_start < 1 or frame_end > frame_count or frame_start > frame_end:
        raise character_svg.CharacterSvgError(
            f"Invalid render batch {frame_start}-{frame_end} for {frame_count} frames"
        )
    layers = character_svg.build_layers(prepared["aliases"], weapon_type=prepared["weapon_type"])
    viewbox = tuple(float(value) for value in prepared["viewbox"])
    if len(viewbox) != 4:
        raise character_svg.CharacterSvgError("Prepare manifest has no usable shared viewbox")
    settings = prepared["settings"]
    max_size = int(settings["max_size"])
    zoom = float(settings["zoom"])
    durations = [int(value) for value in prepared["frame_durations"]]
    records: list[dict[str, Any]] = []
    all_warnings: list[str] = []

    with tempfile.TemporaryDirectory(prefix=f"aqw-render-{job_id}-{batch_index}-") as temporary:
        root = Path(temporary)
        part_roots: dict[str, Path] = {}
        for key, part in prepared["parts"].items():
            archive_path = store.download(
                config.work_bucket,
                part["archive_key"],
                root / "archives" / f"{key}.tar.gz",
            )
            target = root / "parts" / key
            _extract_archive(archive_path, target)
            part_roots[key] = target

        def compose_frame(frame_number: int) -> Path:
            imported: dict[str, character_svg.ImportedSymbol] = {}
            for key, part in prepared["parts"].items():
                raw_path = part_roots[key] / f"{frame_number:06d}.svg"
                if not raw_path.is_file():
                    raise character_svg.CharacterSvgError(
                        f"Part archive for {key} is missing frame {frame_number}"
                    )
                imported[key] = character_svg.import_ffdec_symbol(
                    key,
                    raw_path,
                    zoom=zoom,
                    color_rules={
                        name: tuple(rule) for name, rule in part["color_rules"].items()
                    },
                    root_class=part["root_class"],
                )
            output = root / "svg" / f"{frame_number:06d}.svg"
            warnings = character_svg.compose_svg(
                imported,
                layers,
                fields=prepared["fields"],
                all_color_rules=[tuple(value) for value in prepared["all_color_rules"]],
                output=output,
                max_size=max_size,
                padding=0,
                facing=settings["facing"],
                rsvg_convert=None,
            )
            for warning in warnings:
                if warning not in all_warnings:
                    all_warnings.append(warning)
            return output

        # Compose and rasterize one extra leading frame so the first encoded
        # frame of the batch can delta against its predecessor.
        pngs: dict[int, Path] = {}
        first_needed = frame_start - 1 if frame_start > 1 else frame_start
        for frame_number in range(first_needed, frame_end + 1):
            svg = compose_frame(frame_number)
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
            x, y, width, height, frame_canvas = _encode_frame(
                pngs[frame_number],
                pngs.get(frame_number - 1),
                encoded,
                quality=float(settings["webp_quality"]),
                method=int(settings["webp_method"]),
                cwebp=config.cwebp,
            )
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

    batch_manifest_key = f"jobs/{job_id}/render/batch-{batch_index:04d}.json"
    store.write_json(
        config.work_bucket,
        batch_manifest_key,
        {
            "schema_version": 1,
            "job_id": job_id,
            "batch": batch_index,
            "frames": records,
            "warnings": all_warnings,
        },
    )
    return {
        "job_id": job_id,
        "batch": batch_index,
        "batch_manifest_key": batch_manifest_key,
    }
