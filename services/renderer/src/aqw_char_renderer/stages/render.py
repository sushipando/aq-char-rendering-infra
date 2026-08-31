"""Compose, rasterize, and encode one batch of complete frames in a single stage.

This merged stage replaces the old compose -> bounds -> raster chain. Prepare
computes one shared animation canvas up front, so each worker can compose and
rasterize exactly its configured frames. Complete WebP frames avoid computing
the previous batch's overlap frame solely for delta cropping.
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


def _downsample(source: Path, *, output_size: int) -> tuple[int, int]:
    """Shrink one RGBA raster with premultiplied-alpha Lanczos filtering.

    The no-op path intentionally does not rewrite the PNG. Besides avoiding
    work when raster and output sizes match, this keeps the existing pixel
    output byte-for-byte unchanged.
    """
    with Image.open(source) as image:
        if image.mode != "RGBA":
            raise character_svg.CharacterSvgError("Raster frame is not transparent RGBA")
        image.load()
        width, height = image.size
        longest = max(width, height)
        if longest == output_size:
            return image.size
        if longest < output_size:
            raise character_svg.CharacterSvgError(
                "Output size cannot exceed the raster frame size"
            )
        scale = output_size / longest
        target = (
            max(1, round(width * scale)),
            max(1, round(height * scale)),
        )
        # Filtering premultiplied RGB avoids pulling arbitrary transparent RGB
        # into antialiased edges while the supersampled frame is reduced.
        premultiplied = image.convert("RGBa")

    resized: Image.Image | None = None
    rgba: Image.Image | None = None
    try:
        resized = premultiplied.resize(
            target,
            Image.Resampling.LANCZOS,
            reducing_gap=3.0,
        )
        rgba = resized.convert("RGBA")
        rgba.save(source, format="PNG")
    finally:
        premultiplied.close()
        if resized is not None:
            resized.close()
        if rgba is not None:
            rgba.close()
    return target


def _encode_frame(
    current: Path,
    output: Path,
    *,
    quality: float,
    method: int,
    cwebp: str,
) -> tuple[int, int, int, int, tuple[int, int]]:
    with Image.open(current) as image:
        canvas = image.size
    x, y = 0, 0
    width, height = canvas
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
    """Render one batch: compose each frame from cached vector states,
    rasterize it once at the manifest's shared canvas, and encode complete
    WebP frames. There is no probe/fit phase; the canvas comes from
    prepare_finish's union of each state's alpha-probed visible bounds.
    """
    batch_started = time.perf_counter()
    timings: dict[str, float] = {
        "manifest_ms": 0.0,
        "archive_download_ms": 0.0,
        "archive_extract_ms": 0.0,
        "compose_ms": 0.0,
        "probe_ms": 0.0,
        "rasterize_ms": 0.0,
        "downsample_ms": 0.0,
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
    viewbox = tuple(float(value) for value in prepared["viewbox"])
    if len(viewbox) != 4:
        raise character_svg.CharacterSvgError("Prepare manifest has no usable shared viewbox")
    settings = prepared["settings"]
    raster_size = int(settings["raster_size"])
    output_size = int(settings["output_size"])
    zoom = float(settings["zoom"])
    durations = [int(value) for value in prepared["frame_durations"]]
    records: list[dict[str, Any]] = []
    all_warnings: list[str] = []
    detected_blink_frames = prepared.get("detected_blink_frames")
    ignored_loop_keys = set(prepared.get("ignored_loop_keys") or ())
    static_keys = set(prepared.get("static_keys") or ())
    ground_animate = {
        str(key): int(span)
        for key, span in (prepared.get("ground_animate") or {}).items()
    }

    def source_frame_for(key: str, frame_number: int) -> int:
        span = ground_animate.get(key, 0)
        if span >= 2:
            # Random-pose ground cosmetics bob inside their leading pose span;
            # ping-pong it so the motion stays without the direction flip.
            return character_svg.pingpong_source_frame_index(
                frame_number - 1,
                span=span,
            )
        if key in static_keys:
            # Random-pose ground cosmetics are frozen at their initial pose
            # instead of looping the mid-timeline direction flip.
            return 1
        if detected_blink_frames and key in ignored_loop_keys and detected_blink_frames > 0:
            zero_based = character_svg.one_shot_source_frame_index(
                frame_number - 1,
                one_shot_frames=detected_blink_frames,
            )
            return zero_based + 1
        return frame_number

    with tempfile.TemporaryDirectory(prefix=f"aqw-render-{job_id}-{batch_index}-") as temporary:
        root = Path(temporary)
        part_roots: dict[str, Path] = {}
        archive_bytes = 0
        # Fetch every (source, chunk ordinal) covering only this batch's source
        # frames, then extract each bundle once. Complete-frame encoding does
        # not need the previous batch's frame.
        needed_pairs: set[tuple[int, int]] = set()
        for key, part in prepared["parts"].items():
            source_idx = int(part.get("source_idx", 0))
            source_bundles = (prepared.get("source_bundles") or {}).get(str(source_idx), {})
            if not source_bundles:
                continue
            for output_frame in range(frame_start, frame_end + 1):
                source_frame = source_frame_for(key, output_frame)
                if source_frame >= 1:
                    needed_pairs.add(
                        (
                            source_idx,
                            (source_frame - 1) // config.source_bundle_frame_count,
                        )
                    )
        for source_idx, ordinal in sorted(needed_pairs):
            source_bundles = (prepared.get("source_bundles") or {}).get(str(source_idx), {})
            scoped = source_bundles.get(str(ordinal))
            if not scoped:
                continue
            download_started = time.perf_counter()
            archive_path = store.download(
                config.work_bucket,
                scoped,
                root / "archives" / f"source-{source_idx}-{ordinal}.tar.gz",
            )
            timings["archive_download_ms"] += (time.perf_counter() - download_started) * 1000
            archive_bytes += archive_path.stat().st_size
            extract_started = time.perf_counter()
            _extract_archive(archive_path, root / "parts" / f"src{source_idx}")
            timings["archive_extract_ms"] += (time.perf_counter() - extract_started) * 1000
        for key, part in prepared["parts"].items():
            source_idx = int(part.get("source_idx", 0))
            part_roots[key] = root / "parts" / f"src{source_idx}" / key
        # Fallback for manifests without source bundles: fetch the full
        # per-symbol archive once.
        for key, part in prepared["parts"].items():
            if part_roots[key].is_dir() and any(part_roots[key].glob("*.svg")):
                continue
            legacy_archive_key = part.get("archive_key")
            if not legacy_archive_key:
                raise character_svg.CharacterSvgError(
                    f"Source bundle is missing the requested frames for {key}"
                )
            download_started = time.perf_counter()
            archive_path = store.download(
                config.work_bucket,
                legacy_archive_key,
                root / "archives" / f"{key}.full.tar.gz",
            )
            timings["archive_download_ms"] += (time.perf_counter() - download_started) * 1000
            archive_bytes += archive_path.stat().st_size
            extract_started = time.perf_counter()
            _extract_archive(archive_path, part_roots[key])
            timings["archive_extract_ms"] += (time.perf_counter() - extract_started) * 1000

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
                max_size=raster_size,
                padding=0,
                facing=settings["facing"],
                rsvg_convert=None,
            )
            for warning in warnings:
                if warning not in all_warnings:
                    all_warnings.append(warning)
            return output

        pngs: dict[int, Path] = {}
        for frame_number in range(frame_start, frame_end + 1):
            compose_started = time.perf_counter()
            svg = compose_frame(frame_number)
            timings["compose_ms"] += (time.perf_counter() - compose_started) * 1000
            png = root / "png" / f"{frame_number:06d}.png"
            rasterize_started = time.perf_counter()
            _rasterize(
                svg,
                png,
                viewbox=viewbox,  # type: ignore[arg-type]
                max_size=raster_size,
                rsvg_convert=config.rsvg_convert,
            )
            timings["rasterize_ms"] += (time.perf_counter() - rasterize_started) * 1000
            if output_size < raster_size:
                downsample_started = time.perf_counter()
                _downsample(png, output_size=output_size)
                timings["downsample_ms"] += (
                    time.perf_counter() - downsample_started
                ) * 1000
            pngs[frame_number] = png

        canvas_size: tuple[int, int] | None = None
        for frame_number in range(frame_start, frame_end + 1):
            encoded = root / "webp" / f"{frame_number:06d}.webp"
            encoded.parent.mkdir(parents=True, exist_ok=True)
            encode_started = time.perf_counter()
            x, y, width, height, frame_canvas = _encode_frame(
                pngs[frame_number],
                encoded,
                quality=float(settings["webp_quality"]),
                method=int(settings["webp_method"]),
                cwebp=config.cwebp,
            )
            timings["encode_ms"] += (time.perf_counter() - encode_started) * 1000
            if canvas_size is None:
                canvas_size = frame_canvas
            elif frame_canvas != canvas_size:
                raise character_svg.CharacterSvgError("Raster frames do not share one canvas")
            output_key = f"jobs/{job_id}/webp-frames/{frame_number:06d}.webp"
            upload_started = time.perf_counter()
            store.upload_file(encoded, config.work_bucket, output_key, content_type="image/webp")
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
        timings["manifest_write_ms"] = (time.perf_counter() - manifest_write_started) * 1000

    total_ms = (time.perf_counter() - batch_started) * 1000
    frames_rendered = frame_end - frame_start + 1
    accounted = sum(timings.values())
    log_event(
        "render_batch_profile",
        job_id=job_id,
        batch=batch_index,
        mode="raster",
        frame_start=frame_start,
        frame_end=frame_end,
        frames_rendered=frames_rendered,
        delta_encoded=False,
        overlap_frame=False,
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
        "mode": "raster",
    }
