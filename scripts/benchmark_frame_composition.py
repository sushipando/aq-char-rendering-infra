"""Benchmark component-frame composition using artifacts from one render job.

The artifact directory must contain ``manifest.json`` plus the downloaded
``component/results`` and ``component/rasters`` trees. The benchmark keeps
WebP encoding out of the composition timings and compares an output-grid
Pillow path against the current compose-at-raster-size-then-downsample path.

Example:
    uv run --package aqw-char-renderer python scripts/benchmark_frame_composition.py \
        --artifact-dir /private/tmp/alina-component-benchmark \
        --frames 120
"""

from __future__ import annotations

import argparse
import json
import math
import shutil
import subprocess
import tempfile
from pathlib import Path
from time import perf_counter
from typing import Any

import numpy as np
from aqw_char_renderer.stages.compose_frames import _downsample_image
from PIL import Image


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser(description=__doc__)
    result.add_argument("--artifact-dir", type=Path, required=True)
    result.add_argument("--frames", type=int, default=120)
    result.add_argument("--quality-frames", type=int, default=8)
    result.add_argument("--magick-frames", type=int, default=12)
    result.add_argument("--skip-magick", action="store_true")
    result.add_argument("--json-output", type=Path)
    return result


def load_inputs(
    artifact_dir: Path,
) -> tuple[dict[str, Any], dict[str, dict[str, Any]], dict[str, Image.Image], float]:
    manifest = json.loads((artifact_dir / "manifest.json").read_text(encoding="utf-8"))
    results: dict[str, dict[str, Any]] = {}
    for path in sorted((artifact_dir / "component" / "results").glob("*.json")):
        record = json.loads(path.read_text(encoding="utf-8"))
        results[str(record["task_id"])] = record

    referenced = {
        str(task_id)
        for frame in manifest["component_frames"]
        for task_id in frame["layers"]
    }
    missing = referenced.difference(results)
    if missing:
        raise RuntimeError(f"Missing {len(missing)} component result record(s)")

    started = perf_counter()
    images: dict[str, Image.Image] = {}
    for task_id in sorted(referenced):
        record = results[task_id]
        if record.get("empty") is True:
            continue
        path = artifact_dir / "component" / "rasters" / f"{task_id}.png"
        with Image.open(path) as source:
            image = source.convert("RGBA")
            image.load()
        images[task_id] = image
    return manifest, results, images, perf_counter() - started


def canvas_sizes(manifest: dict[str, Any]) -> tuple[tuple[int, int], tuple[int, int]]:
    viewbox = tuple(float(value) for value in manifest["viewbox"])
    raster_size = int(manifest["settings"]["raster_size"])
    output_size = int(manifest["settings"]["output_size"])
    pixel_scale = raster_size / max(viewbox[2], viewbox[3])
    raster_canvas = (
        max(1, round(viewbox[2] * pixel_scale)),
        max(1, round(viewbox[3] * pixel_scale)),
    )
    output_scale = output_size / raster_size
    output_canvas = (
        max(1, round(raster_canvas[0] * output_scale)),
        max(1, round(raster_canvas[1] * output_scale)),
    )
    return raster_canvas, output_canvas


def compose_pillow(
    frame: dict[str, Any],
    *,
    canvas: tuple[int, int],
    images: dict[str, Image.Image],
    results: dict[str, dict[str, Any]],
) -> Image.Image:
    output = Image.new("RGBA", canvas, (0, 0, 0, 0))
    for raw_task_id in frame["layers"]:
        task_id = str(raw_task_id)
        layer = images.get(task_id)
        if layer is None:
            continue
        record = results[task_id]
        output.alpha_composite(layer, dest=(int(record["x"]), int(record["y"])))
    return output


def _resize_exact(image: Image.Image, size: tuple[int, int]) -> Image.Image:
    premultiplied = image.convert("RGBa")
    try:
        resized = premultiplied.resize(
            size,
            Image.Resampling.LANCZOS,
            reducing_gap=3.0,
        )
        try:
            return resized.convert("RGBA")
        finally:
            resized.close()
    finally:
        premultiplied.close()


def build_output_grid_layers(
    manifest: dict[str, Any],
    results: dict[str, dict[str, Any]],
    images: dict[str, Image.Image],
) -> tuple[dict[str, Image.Image], dict[str, dict[str, Any]], float]:
    """Downsample tight layers without introducing half-pixel placement shifts."""
    raster_size = int(manifest["settings"]["raster_size"])
    output_size = int(manifest["settings"]["output_size"])
    ratio = raster_size / output_size
    factor = round(ratio)
    if not math.isclose(ratio, factor) or factor < 1:
        raise RuntimeError(
            "Output-grid component downsampling currently requires an integer "
            "raster_size/output_size ratio"
        )

    started = perf_counter()
    scaled_images: dict[str, Image.Image] = {}
    scaled_results: dict[str, dict[str, Any]] = {}
    for task_id, source in images.items():
        record = results[task_id]
        x = int(record["x"])
        y = int(record["y"])
        right = x + source.width
        bottom = y + source.height
        aligned_left = math.floor(x / factor) * factor
        aligned_top = math.floor(y / factor) * factor
        aligned_right = math.ceil(right / factor) * factor
        aligned_bottom = math.ceil(bottom / factor) * factor
        padded = Image.new(
            "RGBA",
            (aligned_right - aligned_left, aligned_bottom - aligned_top),
            (0, 0, 0, 0),
        )
        padded.alpha_composite(source, dest=(x - aligned_left, y - aligned_top))
        scaled = _resize_exact(
            padded,
            (
                (aligned_right - aligned_left) // factor,
                (aligned_bottom - aligned_top) // factor,
            ),
        )
        padded.close()
        scaled_images[task_id] = scaled
        scaled_results[task_id] = {
            **record,
            "x": aligned_left // factor,
            "y": aligned_top // factor,
            "width": scaled.width,
            "height": scaled.height,
        }
    return scaled_images, scaled_results, perf_counter() - started


def benchmark_current(
    frames: list[dict[str, Any]],
    *,
    canvas: tuple[int, int],
    output_size: int,
    images: dict[str, Image.Image],
    results: dict[str, dict[str, Any]],
    sample_indices: set[int],
) -> tuple[dict[str, float], dict[int, Image.Image]]:
    composite_seconds = 0.0
    resize_seconds = 0.0
    samples: dict[int, Image.Image] = {}
    for index, frame in enumerate(frames):
        started = perf_counter()
        raster = compose_pillow(
            frame,
            canvas=canvas,
            images=images,
            results=results,
        )
        composite_seconds += perf_counter() - started
        started = perf_counter()
        output = _downsample_image(raster, output_size=output_size)
        resize_seconds += perf_counter() - started
        if output is not raster:
            raster.close()
        if index in sample_indices:
            samples[index] = output
        else:
            output.close()
    return {
        "composite_seconds": composite_seconds,
        "resize_seconds": resize_seconds,
        "total_seconds": composite_seconds + resize_seconds,
    }, samples


def benchmark_output_grid(
    frames: list[dict[str, Any]],
    *,
    canvas: tuple[int, int],
    images: dict[str, Image.Image],
    results: dict[str, dict[str, Any]],
    sample_indices: set[int],
) -> tuple[dict[str, float], dict[int, Image.Image]]:
    started = perf_counter()
    samples: dict[int, Image.Image] = {}
    for index, frame in enumerate(frames):
        output = compose_pillow(
            frame,
            canvas=canvas,
            images=images,
            results=results,
        )
        if index in sample_indices:
            samples[index] = output
        else:
            output.close()
    seconds = perf_counter() - started
    return {"composite_seconds": seconds, "total_seconds": seconds}, samples


def premultiplied_mae(left: Image.Image, right: Image.Image) -> tuple[float, int]:
    a = np.asarray(left, dtype=np.float32) / 255.0
    b = np.asarray(right, dtype=np.float32) / 255.0
    a[..., :3] *= a[..., 3:4]
    b[..., :3] *= b[..., 3:4]
    difference = np.abs(a - b)
    return float(difference.mean()), round(float(difference.max()) * 255)


def save_magick_layers(
    directory: Path,
    images: dict[str, Image.Image],
) -> dict[str, Path]:
    paths: dict[str, Path] = {}
    directory.mkdir(parents=True, exist_ok=True)
    for task_id, image in images.items():
        path = directory / f"{task_id}.png"
        image.save(path, format="PNG")
        paths[task_id] = path
    return paths


def benchmark_magick(
    frames: list[dict[str, Any]],
    *,
    canvas: tuple[int, int],
    paths: dict[str, Path],
    results: dict[str, dict[str, Any]],
    magick: str,
) -> dict[str, float]:
    started = perf_counter()
    for frame in frames:
        command = [magick, "-size", f"{canvas[0]}x{canvas[1]}", "xc:none"]
        for raw_task_id in frame["layers"]:
            task_id = str(raw_task_id)
            path = paths.get(task_id)
            if path is None:
                continue
            record = results[task_id]
            command.extend(
                (
                    "(",
                    str(path),
                    "-geometry",
                    f"{int(record['x']):+d}{int(record['y']):+d}",
                    ")",
                    "-composite",
                )
            )
        command.append("null:")
        subprocess.run(command, check=True, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    seconds = perf_counter() - started
    return {"composite_seconds": seconds, "total_seconds": seconds}


def main() -> int:
    args = parser().parse_args()
    manifest, results, images, decode_seconds = load_inputs(args.artifact_dir)
    all_frames = list(manifest["component_frames"])
    if args.frames < 1 or args.frames > len(all_frames):
        raise SystemExit(f"--frames must be between 1 and {len(all_frames)}")
    frames = all_frames[: args.frames]
    raster_canvas, output_canvas = canvas_sizes(manifest)
    output_size = int(manifest["settings"]["output_size"])
    quality_count = min(args.quality_frames, len(frames))
    sample_indices = (
        {
            round(index * (len(frames) - 1) / max(1, quality_count - 1))
            for index in range(quality_count)
        }
        if quality_count
        else set()
    )

    scaled_images, scaled_results, prescale_seconds = build_output_grid_layers(
        manifest,
        results,
        images,
    )

    current, current_frames = benchmark_current(
        frames,
        canvas=raster_canvas,
        output_size=output_size,
        images=images,
        results=results,
        sample_indices=sample_indices,
    )
    output_grid, output_grid_frames = benchmark_output_grid(
        frames,
        canvas=output_canvas,
        images=scaled_images,
        results=scaled_results,
        sample_indices=sample_indices,
    )

    if sample_indices:
        ordered_sample_indices = sorted(sample_indices)
        quality = [
            premultiplied_mae(current_frames[index], output_grid_frames[index])
            for index in ordered_sample_indices
        ]
        quality_summary: dict[str, Any] = {
            "sample_frames": [index + 1 for index in ordered_sample_indices],
            "premultiplied_rgba_mae": sum(item[0] for item in quality) / len(quality),
            "maximum_channel_error": max(item[1] for item in quality),
        }
    else:
        quality_summary = {}

    magick_result: dict[str, float] | None = None
    magick = shutil.which("magick")
    with tempfile.TemporaryDirectory(prefix="aqw-compose-benchmark-") as temporary:
        if magick is not None and not args.skip_magick and args.magick_frames:
            paths = save_magick_layers(Path(temporary) / "layers", scaled_images)
            magick_result = benchmark_magick(
                frames[: min(args.magick_frames, len(frames))],
                canvas=output_canvas,
                paths=paths,
                results=scaled_results,
                magick=magick,
            )

    for image in current_frames.values():
        image.close()
    for image in output_grid_frames.values():
        image.close()
    for image in images.values():
        image.close()
    for image in scaled_images.values():
        image.close()

    report: dict[str, Any] = {
        "frames": len(frames),
        "layers_per_frame": [
            min(len(frame["layers"]) for frame in frames),
            max(len(frame["layers"]) for frame in frames),
        ],
        "unique_components": len(images),
        "raster_canvas": list(raster_canvas),
        "output_canvas": list(output_canvas),
        "decode_seconds": decode_seconds,
        "output_grid_component_resize_seconds": prescale_seconds,
        "pillow_current": current,
        "pillow_output_grid": output_grid,
        "image_magick_output_grid": magick_result,
        "quality": quality_summary,
    }
    baseline = current["total_seconds"]
    report["speedups"] = {
        "pillow_output_grid_excluding_one_time_component_resize": (
            baseline / output_grid["total_seconds"]
        ),
        "pillow_output_grid_including_one_time_component_resize": (
            baseline / (prescale_seconds + output_grid["total_seconds"])
        ),
    }
    rendered = json.dumps(report, indent=2, sort_keys=True)
    print(rendered)
    if args.json_output is not None:
        args.json_output.parent.mkdir(parents=True, exist_ok=True)
        args.json_output.write_text(rendered + "\n", encoding="utf-8")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
