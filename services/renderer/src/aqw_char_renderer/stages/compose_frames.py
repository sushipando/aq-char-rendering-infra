"""Single-Lambda benchmark compositor: build every final frame from PNG layers.

One invocation reads the component-raster manifest, validates that every
referenced task succeeded, downloads and decodes each unique component PNG
once, then alpha-composites all output frames in back-to-front order at the
recorded integer offsets. New manifests contain already-downsampled
output-grid components; older raster-grid manifests retain their full-frame
premultiplied-alpha shrink. Frames are encoded as WebP, muxed into one
animation, validated, published, and the job completed inline.

No runtime affine transformation or SVG rasterization remains here: that all
happened once per unique placed component state.
"""

from __future__ import annotations

import subprocess
import tempfile
from collections.abc import Mapping
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
from time import perf_counter
from typing import Any, Protocol

from PIL import Image

from aqw_char_renderer import character_svg
from aqw_char_renderer.config import RuntimeConfig
from aqw_char_renderer.hashing import file_sha256
from aqw_char_renderer.stages.finalize import _validate_animation
from aqw_char_renderer.structured_logging import log_event


class StageStore(Protocol):
    def exists(self, bucket: str, key: str) -> Mapping[str, Any] | None: ...
    def download(self, bucket: str, key: str, destination: Path, **kwargs: Any) -> Path: ...
    def upload_file(self, source: Path, bucket: str, key: str, **kwargs: Any) -> None: ...
    def copy(self, bucket: str, source_key: str, destination_key: str, **kwargs: Any) -> None: ...
    def delete(self, bucket: str, key: str) -> None: ...
    def read_json(self, bucket: str, key: str) -> Any: ...


def _frame_canvas_sizes(
    viewbox: tuple[float, float, float, float],
    *,
    raster_size: int,
    output_size: int,
) -> tuple[tuple[int, int], tuple[int, int]]:
    """Return the raster and delivered canvases using the legacy rounding.

    The output dimensions intentionally derive from the rounded raster canvas,
    matching ``_downsample_image`` exactly. This matters for aspect-ratio
    dimensions that become odd at the raster size (for example 697 -> 348).
    """
    if len(viewbox) != 4 or viewbox[2] <= 0 or viewbox[3] <= 0:
        raise character_svg.CharacterSvgError("Prepare manifest has no usable shared viewbox")
    if raster_size <= 0 or output_size <= 0 or output_size > raster_size:
        raise character_svg.CharacterSvgError(
            "Output size must be positive and cannot exceed raster size"
        )
    pixel_scale = raster_size / max(viewbox[2], viewbox[3])
    raster_canvas = (
        max(1, round(viewbox[2] * pixel_scale)),
        max(1, round(viewbox[3] * pixel_scale)),
    )
    if output_size == raster_size:
        return raster_canvas, raster_canvas
    scale = output_size / max(raster_canvas)
    output_canvas = (
        max(1, round(raster_canvas[0] * scale)),
        max(1, round(raster_canvas[1] * scale)),
    )
    return raster_canvas, output_canvas


def _downsample_image(image: Image.Image, *, output_size: int) -> Image.Image:
    """Shrink one RGBA image with premultiplied-alpha Lanczos filtering."""
    if image.mode != "RGBA":
        raise character_svg.CharacterSvgError("Composed frame is not transparent RGBA")
    width, height = image.size
    longest = max(width, height)
    if longest == output_size:
        return image
    if longest < output_size:
        raise character_svg.CharacterSvgError("Output size cannot exceed the raster frame size")
    scale = output_size / longest
    target = (
        max(1, round(width * scale)),
        max(1, round(height * scale)),
    )
    premultiplied = image.convert("RGBa").resize(
        target,
        Image.Resampling.LANCZOS,
        reducing_gap=3.0,
    )
    return premultiplied.convert("RGBA")


def _encode_png_frame(
    png: Path,
    output: Path,
    *,
    quality: float,
    method: int,
    cwebp: str,
) -> None:
    """Encode an existing RGBA PNG with the job's configured WebP settings."""
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
    command.extend((str(png), "-o", str(output)))
    result = subprocess.run(command, capture_output=True, text=True, check=False)
    if result.returncode:
        detail = (result.stderr or result.stdout).strip()
        raise character_svg.CharacterSvgError(f"cwebp failed: {detail[-2000:]}")
    if not output.is_file() or output.stat().st_size == 0:
        raise character_svg.CharacterSvgError("cwebp produced an empty frame")


def _encode_frame(
    image: Image.Image,
    output: Path,
    *,
    quality: float,
    method: int,
    cwebp: str,
) -> None:
    """Encode one composed Pillow frame with the job's WebP settings."""
    png = output.with_suffix(".png")
    image.save(png, format="PNG")
    try:
        _encode_png_frame(
            png,
            output,
            quality=quality,
            method=method,
            cwebp=cwebp,
        )
    finally:
        png.unlink(missing_ok=True)


def compose_all_frames(
    *,
    job_id: str,
    manifest_key: str,
    component_results: list[dict[str, Any]],
    store: StageStore,
    config: RuntimeConfig,
) -> dict[str, Any]:
    """Compose, encode, mux, validate, and publish all component frames."""
    started = perf_counter()
    timings: dict[str, float] = {
        "manifest_ms": 0.0,
        "results_read_ms": 0.0,
        "png_download_ms": 0.0,
        "decode_ms": 0.0,
        "composite_ms": 0.0,
        "downsample_ms": 0.0,
        "encode_ms": 0.0,
        "mux_ms": 0.0,
        "validation_ms": 0.0,
        "publish_ms": 0.0,
    }
    manifest_started = perf_counter()
    prepared = store.read_json(config.work_bucket, manifest_key)
    timings["manifest_ms"] = (perf_counter() - manifest_started) * 1000
    if prepared.get("job_id") != job_id:
        raise character_svg.CharacterSvgError("Prepare manifest belongs to another job")
    if not prepared.get("component_pipeline"):
        raise character_svg.CharacterSvgError("Prepare manifest is not a component pipeline")
    frame_count = int(prepared["frame_count"])
    viewbox = tuple(float(value) for value in prepared["viewbox"])
    if len(viewbox) != 4:
        raise character_svg.CharacterSvgError("Prepare manifest has no usable shared viewbox")
    settings = prepared["settings"]
    raster_size = int(settings["raster_size"])
    output_size = int(settings["output_size"])
    component_frames = prepared["component_frames"]
    if len(component_frames) != frame_count:
        raise character_svg.CharacterSvgError(
            "Component frame count does not match the prepared frame count"
        )
    durations = [int(value) for value in prepared["frame_durations"]]

    results_started = perf_counter()
    results_by_task = {str(result["task_id"]): result for result in component_results}
    referenced: set[str] = set()
    for frame in component_frames:
        referenced.update(str(task_id) for task_id in frame["layers"])
    missing = sorted(referenced.difference(results_by_task))
    if missing:
        preview = ", ".join(missing[:5])
        if len(missing) > 5:
            preview += ", ..."
        raise character_svg.CharacterSvgError(
            f"{len(missing)} component task(s) missing results: {preview}"
        )
    timings["results_read_ms"] = (perf_counter() - results_started) * 1000

    raster_canvas, output_canvas = _frame_canvas_sizes(
        viewbox,
        raster_size=raster_size,
        output_size=output_size,
    )
    declared_raster_space = prepared.get("component_raster_space")
    component_raster_space = str(declared_raster_space or "raster")
    if component_raster_space == "output":
        canvas_size = output_canvas
    elif component_raster_space == "raster":
        canvas_size = raster_canvas
    else:
        raise character_svg.CharacterSvgError(
            f"Unsupported component raster space {component_raster_space!r}"
        )
    for result in component_results:
        if result.get("empty") is True:
            continue
        result_space = result.get("component_raster_space")
        if declared_raster_space is not None and result_space is None:
            raise character_svg.CharacterSvgError(
                f"Component task {result.get('task_id')} has no coordinate space"
            )
        if result_space is not None and str(result_space) != component_raster_space:
            raise character_svg.CharacterSvgError(
                f"Component task {result.get('task_id')} uses {result_space!r} "
                f"coordinates, expected {component_raster_space!r}"
            )

    with tempfile.TemporaryDirectory(prefix=f"aqw-compose-{job_id[:8]}-") as temporary:
        root = Path(temporary)
        # Download and decode each unique component PNG exactly once,
        # concurrently with a bounded worker pool.
        unique_results = [
            result
            for result in results_by_task.values()
            if result.get("empty") is not True and result.get("png_key")
        ]
        images: dict[str, Image.Image] = {}
        png_bytes = 0
        download_started = perf_counter()

        def fetch(result: dict[str, Any]) -> tuple[str, Path]:
            return (
                str(result["task_id"]),
                store.download(
                    config.work_bucket,
                    str(result["png_key"]),
                    root / "rasters" / f"{result['task_id']}.png",
                    expected_sha256=result.get("sha256"),
                ),
            )

        workers = min(
            config.finalizer_download_concurrency,
            max(1, len(unique_results)),
        )
        with ThreadPoolExecutor(max_workers=workers, thread_name_prefix="component") as executor:
            fetched = list(executor.map(fetch, unique_results))
        timings["png_download_ms"] = (perf_counter() - download_started) * 1000
        for path in (item[1] for item in fetched):
            png_bytes += path.stat().st_size
        decode_started = perf_counter()
        for task_id, path in fetched:
            with Image.open(path) as image:
                loaded = image.convert("RGBA")
                loaded.load()
            images[task_id] = loaded
        timings["decode_ms"] = (perf_counter() - decode_started) * 1000

        # Composite every frame into one shared canvas and encode it.
        records: list[dict[str, Any]] = []
        composite_total = 0.0
        downsample_total = 0.0
        encode_total = 0.0
        frame_canvases: set[tuple[int, int]] = set()
        for frame in component_frames:
            frame_number = int(frame["number"])
            canvas = Image.new("RGBA", canvas_size, (0, 0, 0, 0))
            composite_started = perf_counter()
            for task_id in frame["layers"]:
                result = results_by_task.get(str(task_id))
                if result is None or result.get("empty") is True:
                    continue
                layer = images.get(str(task_id))
                if layer is None:
                    raise character_svg.CharacterSvgError(
                        f"Component PNG for task {task_id} was not decoded"
                    )
                # Match SVG's normal source-over layer composition.  Using
                # ``paste(..., mask=layer)`` here blends the source alpha into
                # the destination alpha a second time, making antialiased
                # edges and translucent artwork too transparent.
                canvas.alpha_composite(
                    layer,
                    dest=(int(result["x"]), int(result["y"])),
                )
            composite_total += perf_counter() - composite_started
            if component_raster_space == "raster" and output_size < raster_size:
                downsample_started = perf_counter()
                canvas = _downsample_image(canvas, output_size=output_size)
                downsample_total += perf_counter() - downsample_started
            frame_canvas = canvas.size
            frame_canvases.add(frame_canvas)
            encoded = root / "webp" / f"{frame_number:06d}.webp"
            encoded.parent.mkdir(parents=True, exist_ok=True)
            encode_started = perf_counter()
            _encode_frame(
                canvas,
                encoded,
                quality=float(settings["webp_quality"]),
                method=int(settings["webp_method"]),
                cwebp=config.cwebp,
            )
            encode_total += perf_counter() - encode_started
            canvas.close()
            records.append(
                {
                    "frame": frame_number,
                    "x": 0,
                    "y": 0,
                    "width": frame_canvas[0],
                    "height": frame_canvas[1],
                    "canvas_width": frame_canvas[0],
                    "canvas_height": frame_canvas[1],
                    "duration": int(frame.get("duration_ms") or durations[frame_number - 1]),
                    "sha256": file_sha256(encoded),
                    "bytes": encoded.stat().st_size,
                }
            )
        if len(frame_canvases) != 1:
            raise character_svg.CharacterSvgError("Composed frames do not share one canvas")
        canvas = next(iter(frame_canvases))
        timings["composite_ms"] = composite_total * 1000
        timings["downsample_ms"] = downsample_total * 1000
        timings["encode_ms"] += encode_total * 1000
        final_key = str(prepared["final_key"])
        output = root / "result.webp"
        mux_started = perf_counter()
        command = [config.webpmux]
        for frame in records:
            command.extend(
                (
                    "-frame",
                    str(root / "webp" / f"{frame['frame']:06d}.webp"),
                    f"+{int(frame['duration'])}+0+0+0-b",
                )
            )
        command.extend(("-loop", "0", "-bgcolor", "0,0,0,0", "-o", str(output)))
        result = subprocess.run(command, capture_output=True, text=True, check=False)
        if result.returncode:
            detail = (result.stderr or result.stdout).strip()
            raise character_svg.CharacterSvgError(f"webpmux failed: {detail[-2000:]}")
        timings["mux_ms"] = (perf_counter() - mux_started) * 1000
        validation_started = perf_counter()
        _validate_animation(output, frame_count=frame_count, canvas=canvas)
        timings["validation_ms"] = (perf_counter() - validation_started) * 1000
        duration_ms = sum(int(frame["duration"]) for frame in records)
        temporary_key = f"jobs/{job_id}/final/result.webp"
        metadata = {
            "render-hash": str(prepared["render_hash"]),
            "frame-count": str(frame_count),
            "width": str(canvas[0]),
            "height": str(canvas[1]),
            "duration-ms": str(duration_ms),
            "sha256": file_sha256(output),
        }
        publish_started = perf_counter()
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
        timings["publish_ms"] = (perf_counter() - publish_started) * 1000
        final_bytes = output.stat().st_size

    total_ms = (perf_counter() - started) * 1000
    accounted = sum(timings.values())
    log_event(
        "compose_all_profile",
        job_id=job_id,
        frame_count=frame_count,
        task_count=len(referenced),
        unique_png_count=len(unique_results),
        canvas_width=canvas_size[0],
        canvas_height=canvas_size[1],
        raster_size=raster_size,
        output_size=output_size,
        component_raster_space=component_raster_space,
        downsampled=component_raster_space == "raster" and output_size < raster_size,
        png_bytes=png_bytes,
        output_bytes=final_bytes,
        download_concurrency=workers,
        ms_per_frame=round(total_ms / frame_count, 1),
        total_ms=round(total_ms, 1),
        unaccounted_ms=round(total_ms - accounted, 1),
        **{key: round(value, 1) for key, value in timings.items()},
    )
    return {
        "url": f"{config.public_base_url}/{final_key}",
        "frame_count": frame_count,
        "width": canvas[0],
        "height": canvas[1],
        "duration_ms": duration_ms,
        "bytes": final_bytes,
        "cache_hit": False,
        "render_hash": prepared["render_hash"],
        "final_key": final_key,
    }
