"""Compose and encode one chunk of frames from cached component rasters."""

from __future__ import annotations

import tempfile
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
from time import perf_counter
from typing import Any, Protocol

from PIL import Image

from aqw_char_renderer import character_svg
from aqw_char_renderer.config import RuntimeConfig
from aqw_char_renderer.hashing import file_sha256
from aqw_char_renderer.stages.compose_frames import (
    _downsample_image,
    _encode_frame,
    _encode_png_frame,
    _frame_canvas_sizes,
)
from aqw_char_renderer.structured_logging import log_event


class StageStore(Protocol):
    def download(self, bucket: str, key: str, destination: Path, **kwargs: Any) -> Path: ...
    def upload_file(self, source: Path, bucket: str, key: str, **kwargs: Any) -> None: ...
    def read_json(self, bucket: str, key: str) -> Any: ...
    def write_json(self, bucket: str, key: str, value: Any) -> None: ...


def compose_frame_batch(
    *,
    job_id: str,
    manifest_key: str,
    component_results: list[dict[str, Any]],
    batch: dict[str, int],
    store: StageStore,
    config: RuntimeConfig,
    compositor: str | None = None,
) -> dict[str, Any]:
    """Compose, encode, and upload one contiguous output-size frame chunk.

    New manifests provide component PNGs already aligned to the output grid.
    The raster-grid downsample branch remains for older saved manifests.
    """
    started = perf_counter()
    selected_compositor = (compositor or config.component_compositor).strip().casefold()
    if selected_compositor not in {"pillow", "pyvips"}:
        raise character_svg.CharacterSvgError(
            f"Unsupported component compositor {selected_compositor!r}"
        )
    timings: dict[str, float] = {
        "manifest_ms": 0.0,
        "results_read_ms": 0.0,
        "png_download_ms": 0.0,
        "decode_ms": 0.0,
        "composite_ms": 0.0,
        "downsample_ms": 0.0,
        "encode_ms": 0.0,
        "upload_ms": 0.0,
        "manifest_write_ms": 0.0,
    }

    phase = perf_counter()
    prepared = store.read_json(config.work_bucket, manifest_key)
    timings["manifest_ms"] = (perf_counter() - phase) * 1000
    if prepared.get("job_id") != job_id:
        raise character_svg.CharacterSvgError("Prepare manifest belongs to another job")
    if not prepared.get("component_pipeline"):
        raise character_svg.CharacterSvgError("Prepare manifest is not a component pipeline")

    batch_index = int(batch["index"])
    frame_start = int(batch["frame_start"])
    frame_end = int(batch["frame_end"])
    frame_count = int(prepared["frame_count"])
    if frame_start < 1 or frame_end > frame_count or frame_start > frame_end:
        raise character_svg.CharacterSvgError(
            f"Invalid component compose batch {frame_start}-{frame_end} for {frame_count} frames"
        )

    viewbox = tuple(float(value) for value in prepared["viewbox"])
    if len(viewbox) != 4:
        raise character_svg.CharacterSvgError("Prepare manifest has no usable shared viewbox")
    settings = prepared["settings"]
    raster_size = int(settings["raster_size"])
    output_size = int(settings["output_size"])
    durations = [int(value) for value in prepared["frame_durations"]]
    all_component_frames = prepared["component_frames"]
    if len(all_component_frames) != frame_count:
        raise character_svg.CharacterSvgError(
            "Component frame count does not match the prepared frame count"
        )
    component_frames = all_component_frames[frame_start - 1 : frame_end]
    if [int(frame["number"]) for frame in component_frames] != list(
        range(frame_start, frame_end + 1)
    ):
        raise character_svg.CharacterSvgError("Component frame chunk is not contiguous")

    phase = perf_counter()
    results_by_task = {str(result["task_id"]): result for result in component_results}
    referenced = {str(task_id) for frame in component_frames for task_id in frame["layers"]}
    missing = sorted(referenced.difference(results_by_task))
    if missing:
        preview = ", ".join(missing[:5])
        if len(missing) > 5:
            preview += ", ..."
        raise character_svg.CharacterSvgError(
            f"{len(missing)} component task(s) missing results: {preview}"
        )
    timings["results_read_ms"] = (perf_counter() - phase) * 1000

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

    with tempfile.TemporaryDirectory(
        prefix=f"aqw-component-compose-{job_id[:8]}-{batch_index}-"
    ) as temporary:
        root = Path(temporary)
        unique_results = [
            results_by_task[task_id]
            for task_id in sorted(referenced)
            if results_by_task[task_id].get("empty") is not True
            and results_by_task[task_id].get("png_key")
        ]
        images: dict[str, Any] = {}
        png_bytes = 0

        phase = perf_counter()

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

        workers = min(config.finalizer_download_concurrency, max(1, len(unique_results)))
        with ThreadPoolExecutor(max_workers=workers, thread_name_prefix="component") as executor:
            fetched = list(executor.map(fetch, unique_results))
        timings["png_download_ms"] = (perf_counter() - phase) * 1000
        for _task_id, path in fetched:
            png_bytes += path.stat().st_size

        phase = perf_counter()
        if selected_compositor == "pillow":
            for task_id, path in fetched:
                with Image.open(path) as source:
                    image = source.convert("RGBA")
                    image.load()
                images[task_id] = image
        else:
            import pyvips

            # Component images are explicitly retained in memory for the
            # chunk. Disable libvips' global operation cache so completed
            # 4096px frame graphs cannot accumulate across warm invocations.
            pyvips.cache_set_max(0)
            for task_id, path in fetched:
                images[task_id] = pyvips.Image.new_from_file(
                    str(path),
                    access="sequential",
                ).copy_memory()
        timings["decode_ms"] = (perf_counter() - phase) * 1000

        records: list[dict[str, Any]] = []
        frame_canvases: set[tuple[int, int]] = set()
        try:
            for frame in component_frames:
                frame_number = int(frame["number"])
                encoded = root / "webp" / f"{frame_number:06d}.webp"
                encoded.parent.mkdir(parents=True, exist_ok=True)
                if selected_compositor == "pillow":
                    canvas = Image.new("RGBA", canvas_size, (0, 0, 0, 0))
                    phase = perf_counter()
                    for raw_task_id in frame["layers"]:
                        task_id = str(raw_task_id)
                        result = results_by_task[task_id]
                        if result.get("empty") is True:
                            continue
                        layer = images.get(task_id)
                        if layer is None:
                            raise character_svg.CharacterSvgError(
                                f"Component PNG for task {task_id} was not decoded"
                            )
                        canvas.alpha_composite(
                            layer,
                            dest=(int(result["x"]), int(result["y"])),
                        )
                    timings["composite_ms"] += (perf_counter() - phase) * 1000

                    if component_raster_space == "raster" and output_size < raster_size:
                        phase = perf_counter()
                        downsampled = _downsample_image(canvas, output_size=output_size)
                        timings["downsample_ms"] += (perf_counter() - phase) * 1000
                        if downsampled is not canvas:
                            canvas.close()
                        canvas = downsampled

                    frame_canvas = canvas.size
                    phase = perf_counter()
                    _encode_frame(
                        canvas,
                        encoded,
                        quality=float(settings["webp_quality"]),
                        method=int(settings["webp_method"]),
                        cwebp=config.cwebp,
                    )
                    timings["encode_ms"] += (perf_counter() - phase) * 1000
                    canvas.close()
                else:
                    import pyvips

                    phase = perf_counter()
                    layers = []
                    x_positions = []
                    y_positions = []
                    for raw_task_id in frame["layers"]:
                        task_id = str(raw_task_id)
                        result = results_by_task[task_id]
                        if result.get("empty") is True:
                            continue
                        layer = images.get(task_id)
                        if layer is None:
                            raise character_svg.CharacterSvgError(
                                f"Component PNG for task {task_id} was not decoded"
                            )
                        layers.append(layer)
                        x_positions.append(int(result["x"]))
                        y_positions.append(int(result["y"]))
                    base = (
                        pyvips.Image.black(canvas_size[0], canvas_size[1], bands=4)
                        .cast("uchar")
                        .copy(interpretation="srgb")
                    )
                    canvas = base.composite(
                        layers,
                        ["over"] * len(layers),
                        x=x_positions,
                        y=y_positions,
                        premultiplied=False,
                    ).copy_memory()
                    timings["composite_ms"] += (perf_counter() - phase) * 1000

                    if component_raster_space == "raster" and output_size < raster_size:
                        phase = perf_counter()
                        scale = output_size / max(canvas.width, canvas.height)
                        target = (
                            max(1, round(canvas.width * scale)),
                            max(1, round(canvas.height * scale)),
                        )
                        canvas = (
                            canvas.premultiply(max_alpha=255)
                            .resize(
                                target[0] / canvas.width,
                                vscale=target[1] / canvas.height,
                                kernel="lanczos3",
                                gap=3.0,
                            )
                            .unpremultiply(max_alpha=255)
                            .cast("uchar")
                            .copy_memory()
                        )
                        timings["downsample_ms"] += (perf_counter() - phase) * 1000

                    frame_canvas = (canvas.width, canvas.height)
                    png = encoded.with_suffix(".png")
                    phase = perf_counter()
                    canvas.pngsave(str(png), compression=6, strip=True)
                    _encode_png_frame(
                        png,
                        encoded,
                        quality=float(settings["webp_quality"]),
                        method=int(settings["webp_method"]),
                        cwebp=config.cwebp,
                    )
                    timings["encode_ms"] += (perf_counter() - phase) * 1000
                    png.unlink(missing_ok=True)

                frame_canvases.add(frame_canvas)

                output_key = f"jobs/{job_id}/component/webp-frames/{frame_number:06d}.webp"
                phase = perf_counter()
                store.upload_file(
                    encoded,
                    config.work_bucket,
                    output_key,
                    content_type="image/webp",
                )
                timings["upload_ms"] += (perf_counter() - phase) * 1000
                records.append(
                    {
                        "frame": frame_number,
                        "webp_key": output_key,
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
        finally:
            if selected_compositor == "pillow":
                for image in images.values():
                    image.close()

        if len(frame_canvases) != 1:
            raise character_svg.CharacterSvgError("Composed frames do not share one canvas")
        batch_manifest_key = f"jobs/{job_id}/component/compose-batches/batch-{batch_index:04d}.json"
        phase = perf_counter()
        store.write_json(
            config.work_bucket,
            batch_manifest_key,
            {
                "schema_version": 1,
                "job_id": job_id,
                "batch": batch_index,
                "frames": records,
                "warnings": [],
            },
        )
        timings["manifest_write_ms"] = (perf_counter() - phase) * 1000

    total_ms = (perf_counter() - started) * 1000
    rendered_count = frame_end - frame_start + 1
    accounted_ms = sum(timings.values())
    log_event(
        "component_compose_profile",
        job_id=job_id,
        batch=batch_index,
        frame_start=frame_start,
        frame_end=frame_end,
        frames_rendered=rendered_count,
        referenced_component_count=len(referenced),
        downloaded_png_count=len(unique_results),
        png_bytes=png_bytes,
        canvas_width=canvas_size[0],
        canvas_height=canvas_size[1],
        raster_size=raster_size,
        output_size=output_size,
        component_raster_space=component_raster_space,
        downsampled_in_compose=(component_raster_space == "raster" and output_size < raster_size),
        compositor=selected_compositor,
        download_concurrency=workers,
        total_ms=round(total_ms, 1),
        unaccounted_ms=round(total_ms - accounted_ms, 1),
        ms_per_frame=round(total_ms / rendered_count, 1),
        **{key: round(value, 1) for key, value in timings.items()},
    )
    return {
        "job_id": job_id,
        "batch": batch_index,
        "batch_manifest_key": batch_manifest_key,
        "mode": "component-raster",
    }
