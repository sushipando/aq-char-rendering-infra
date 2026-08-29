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
import time
import xml.etree.ElementTree as ET
from pathlib import Path
from typing import Any, Protocol

from PIL import Image

from aqw_char_renderer import character_svg
from aqw_char_renderer.config import RuntimeConfig
from aqw_char_renderer.hashing import file_sha256
from aqw_char_renderer.legacy import render_swf_items as item_renderer
from aqw_char_renderer.structured_logging import log_event


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
    mode: str = "raster",
    store_viewbox_key: str | None = None,
) -> dict[str, Any]:
    """Render one batch.

    mode="probe" composes each frame, computes its tight visible viewbox via
    a low-res alpha probe, uploads the composed SVG, and records per-frame
    tight bounds. mode="raster" downloads the composed SVGs, applies the
    globally fitted canvas from the manifest, rasterizes once, and delta-
    encodes the WebP frames.
    """
    batch_started = time.perf_counter()
    timings: dict[str, float] = {
        "manifest_ms": 0.0,
        "archive_download_ms": 0.0,
        "archive_extract_ms": 0.0,
        "compose_ms": 0.0,
        "rasterize_ms": 0.0,
        "encode_ms": 0.0,
        "upload_ms": 0.0,
    }
    manifest_started = time.perf_counter()
    prepared = store.read_json(config.work_bucket, manifest_key)
    timings["manifest_ms"] = (time.perf_counter() - manifest_started) * 1000
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
    if mode == "raster" and store_viewbox_key:
        fitted = store.read_json(
            config.work_bucket, store_viewbox_key
        )
        if fitted.get("job_id") != job_id:
            raise character_svg.CharacterSvgError(
                "Fitted canvas belongs to another job"
            )
        viewbox = tuple(float(value) for value in fitted["viewbox"])
    else:
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
        archive_bytes = 0
        for key, part in prepared["parts"].items():
            download_started = time.perf_counter()
            archive_path = store.download(
                config.work_bucket,
                part["archive_key"],
                root / "archives" / f"{key}.tar.gz",
            )
            timings["archive_download_ms"] += (time.perf_counter() - download_started) * 1000
            archive_bytes += archive_path.stat().st_size
            target = root / "parts" / key
            extract_started = time.perf_counter()
            _extract_archive(archive_path, target)
            timings["archive_extract_ms"] += (time.perf_counter() - extract_started) * 1000
            part_roots[key] = target

        # Blink timelines play once and then hold their final frame, so item
        # loops (not the eye blink) drive the animation period.
        detected_blink_frames = prepared.get("detected_blink_frames")
        ignored_loop_keys = set(prepared.get("ignored_loop_keys") or ())

        def source_frame_for(key: str, frame_number: int) -> int:
            if (
                detected_blink_frames
                and key in ignored_loop_keys
                and detected_blink_frames > 0
            ):
                # one_shot_source_frame_index is 0-based; convert between the
                # 1-based archive file names and the 0-based blink timeline.
                zero_based = character_svg.one_shot_source_frame_index(
                    frame_number - 1,
                    one_shot_frames=detected_blink_frames,
                )
                return zero_based + 1
            return frame_number

        def compose_frame(frame_number: int) -> Path:
            imported: dict[str, character_svg.ImportedSymbol] = {}
            for key, part in prepared["parts"].items():
                source_frame = source_frame_for(key, frame_number)
                raw_path = part_roots[key] / f"{source_frame:06d}.svg"
                if not raw_path.is_file():
                    raise character_svg.CharacterSvgError(
                        f"Part archive for {key} is missing frame {source_frame}"
                    )
                placement_colors = {
                    tuple(int(component) for component in pair.split(",")):
                    character_svg.AuthoredColorTransform(**values)
                    for pair, values in part.get("placement_colors", {}).items()
                }
                imported[key] = character_svg.import_ffdec_symbol(
                    key,
                    raw_path,
                    zoom=zoom,
                    color_rules={
                        name: tuple(rule) for name, rule in part["color_rules"].items()
                    },
                    root_class=part["root_class"],
                    placement_colors=placement_colors,
                    root_character_id=part.get("character_id"),
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
        # frame of the batch can delta against its predecessor. The overlap
        # frame is never reported for bounds (it belongs to the previous
        # batch), but it IS uploaded so the raster phase can delta against it.
        pngs: dict[int, Path] = {}
        first_needed = frame_start - 1 if frame_start > 1 else frame_start
        svg_cache: dict[int, Path] = {}
        frame_bounds: dict[int, tuple[float, float, float, float]] = {}
        for frame_number in range(first_needed, frame_end + 1):
            if mode == "probe":
                compose_started = time.perf_counter()
                svg = compose_frame(frame_number)
                timings["compose_ms"] += (time.perf_counter() - compose_started) * 1000
                # Probe the composed SVG to get the tight visible bounds.
                tight = item_renderer.detect_svg_visible_viewbox(
                    svg, config.rsvg_convert, probe_size=max(1024, max_size * 2)
                )
                # Fall back to the prepare-time vector canvas if the probe
                # fails for this frame; FitCanvas unions across frames anyway.
                if frame_number >= frame_start:
                    frame_bounds[frame_number] = (
                        tight if tight is not None else viewbox
                    )
                # Upload the composed SVG so the raster phase reuses it.
                svg_key = f"jobs/{job_id}/svg/{frame_number:06d}.svg"
                store.upload_file(
                    svg, config.work_bucket, svg_key, content_type="image/svg+xml"
                )
                svg_cache[frame_number] = svg
            else:
                svg_key = f"jobs/{job_id}/svg/{frame_number:06d}.svg"
                store.download(
                    config.work_bucket,
                    svg_key,
                    root / "svg" / f"{frame_number:06d}.svg",
                )
                svg_cache[frame_number] = root / "svg" / f"{frame_number:06d}.svg"

        if mode == "probe":
            # Record per-frame tight bounds; raster phase will consume them.
            batch_manifest_key = f"jobs/{job_id}/probe/batch-{batch_index:04d}.json"
            manifest_write_started = time.perf_counter()
            store.write_json(
                config.work_bucket,
                batch_manifest_key,
                {
                    "schema_version": 1,
                    "job_id": job_id,
                    "batch": batch_index,
                    "frames": [
                        {"frame": n, "bounds": list(frame_bounds[n])}
                        for n in sorted(frame_bounds)
                    ],
                    "warnings": all_warnings,
                },
            )
            timings["manifest_write_ms"] = (
                time.perf_counter() - manifest_write_started
            ) * 1000
        else:
            # Rasterize each frame at the fitted (tight) canvas.
            for frame_number in range(first_needed, frame_end + 1):
                png = root / "png" / f"{frame_number:06d}.png"
                rasterize_started = time.perf_counter()
                _rasterize(
                    svg_cache[frame_number],
                    png,
                    viewbox=viewbox,  # type: ignore[arg-type]
                    max_size=max_size,
                    rsvg_convert=config.rsvg_convert,
                )
                timings["rasterize_ms"] += (
                    time.perf_counter() - rasterize_started
                ) * 1000
                pngs[frame_number] = png

            canvas_size: tuple[int, int] | None = None
            for frame_number in range(frame_start, frame_end + 1):
                encoded = root / "webp" / f"{frame_number:06d}.webp"
                encoded.parent.mkdir(parents=True, exist_ok=True)
                encode_started = time.perf_counter()
                x, y, width, height, frame_canvas = _encode_frame(
                    pngs[frame_number],
                    pngs.get(frame_number - 1),
                    encoded,
                    quality=float(settings["webp_quality"]),
                    method=int(settings["webp_method"]),
                    cwebp=config.cwebp,
                )
                timings["encode_ms"] += (time.perf_counter() - encode_started) * 1000
                if canvas_size is None:
                    canvas_size = frame_canvas
                elif frame_canvas != canvas_size:
                    raise character_svg.CharacterSvgError(
                        "Raster frames do not share one canvas"
                    )
                output_key = f"jobs/{job_id}/webp-frames/{frame_number:06d}.webp"
                upload_started = time.perf_counter()
                store.upload_file(
                    encoded, config.work_bucket, output_key, content_type="image/webp"
                )
                timings["upload_ms"] += (time.perf_counter() - upload_started) * 1000
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
            manifest_write_started = time.perf_counter()
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
            timings["manifest_write_ms"] = (
                time.perf_counter() - manifest_write_started
            ) * 1000

    total_ms = (time.perf_counter() - batch_started) * 1000
    frames_rendered = frame_end - first_needed + 1
    accounted = sum(timings.values())
    log_event(
        "render_batch_profile",
        job_id=job_id,
        batch=batch_index,
        mode=mode,
        frame_start=frame_start,
        frame_end=frame_end,
        frames_rendered=frames_rendered,
        overlap_frame=frame_start > 1,
        part_count=len(prepared["parts"]),
        archive_bytes=archive_bytes,
        total_ms=round(total_ms, 1),
        unaccounted_ms=round(total_ms - accounted, 1),
        ms_per_frame=round(total_ms / frames_rendered, 1),
        **{key: round(value, 1) for key, value in timings.items()},
    )
    return {
        "job_id": job_id,
        "batch": batch_index,
        "batch_manifest_key": batch_manifest_key,
        "mode": mode,
    }
