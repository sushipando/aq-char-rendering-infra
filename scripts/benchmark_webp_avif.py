#!/usr/bin/env python3
"""Benchmark transparent animated WebP and AVIF from the same decoded frames.

The default run extracts the input losslessly and benchmarks evenly spaced
frames independently. Pass ``--full-animation`` to additionally encode the
whole sequence with img2webp and avifenc, preserving every frame duration.

Examples:

    uv run --package aqw-char-renderer python scripts/benchmark_webp_avif.py

    scripts/benchmark-webp-avif ~/Desktop/annie-large.webp --full-animation
"""

from __future__ import annotations

import argparse
import hashlib
import json
import math
import shutil
import subprocess
import time
from dataclasses import asdict, dataclass
from datetime import UTC, datetime
from pathlib import Path
from typing import Any

import numpy as np
from PIL import Image, features


@dataclass(frozen=True)
class CodecConfig:
    name: str
    family: str
    lossless: bool
    extension: str


@dataclass
class BenchmarkResult:
    name: str
    scope: str
    frames: int
    seconds: float
    bytes: int
    bytes_per_frame: float
    megapixels_per_second: float
    psnr_db: float | None
    ssim_8x8: float | None
    mae: float | None
    alpha_exact: bool | None
    rgba_exact: bool | None
    output: str


CONFIGS = (
    CodecConfig("webp-lossy", "webp", False, ".webp"),
    CodecConfig("webp-lossless", "webp", True, ".webp"),
    CodecConfig("avif-lossy", "avif", False, ".avif"),
    CodecConfig("avif-lossless", "avif", True, ".avif"),
)


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser(
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    result.add_argument(
        "input",
        nargs="?",
        type=Path,
        default=Path.home() / "Desktop" / "annie-large.webp",
        help="animated WebP to decode (default: ~/Desktop/annie-large.webp)",
    )
    result.add_argument(
        "--output-dir",
        type=Path,
        help="new directory for frames and results (default: tmp/<name>-codec-bench-<time>)",
    )
    result.add_argument(
        "--sample-frames",
        type=int,
        default=12,
        help="number of evenly spaced independent frames to benchmark (default: 12)",
    )
    result.add_argument("--webp-quality", type=float, default=85)
    result.add_argument("--webp-method", type=int, choices=range(7), default=4)
    result.add_argument(
        "--avif-quality",
        type=int,
        default=60,
        metavar="0..100",
        help="AVIF lossy quality; Q60 roughly matches WebP Q85 on AQW art",
    )
    result.add_argument("--avif-speed", type=int, default=6, metavar="0..10")
    result.add_argument("--avif-jobs", default="all")
    result.add_argument(
        "--full-animation",
        action="store_true",
        help="also benchmark inter-frame compression for the entire animation",
    )
    result.add_argument(
        "--default-duration-ms",
        type=int,
        default=42,
        help="duration used only when a source frame has no duration metadata",
    )
    return result


def _run(command: list[str]) -> subprocess.CompletedProcess[str]:
    completed = subprocess.run(command, text=True, capture_output=True, check=False)
    if completed.returncode != 0:
        detail = (completed.stderr or completed.stdout).strip()
        raise RuntimeError(
            f"command failed ({completed.returncode}): {' '.join(command)}\n{detail}"
        )
    return completed


def _timed(command: list[str]) -> float:
    started = time.perf_counter()
    _run(command)
    return time.perf_counter() - started


def _tool(name: str) -> str:
    resolved = shutil.which(name)
    if resolved is None:
        raise SystemExit(f"required executable is not installed: {name}")
    return resolved


def _version(command: list[str]) -> str:
    completed = _run(command)
    return (completed.stdout or completed.stderr).strip().splitlines()[0]


def _sample_indices(frame_count: int, requested: int) -> list[int]:
    if requested < 1:
        raise SystemExit("--sample-frames must be at least 1")
    count = min(frame_count, requested)
    if count == 1:
        return [0]
    return sorted(
        {round(index * (frame_count - 1) / (count - 1)) for index in range(count)}
    )


def _output_directory(input_path: Path, requested: Path | None) -> Path:
    if requested is not None:
        output = requested.expanduser().resolve()
    else:
        stamp = datetime.now(UTC).strftime("%Y%m%d-%H%M%S")
        output = (Path("tmp") / f"{input_path.stem}-codec-bench-{stamp}").resolve()
    if output.exists():
        raise SystemExit(f"output directory already exists: {output}")
    output.mkdir(parents=True)
    return output


def extract_frames(
    input_path: Path,
    frames_dir: Path,
    default_duration_ms: int,
) -> tuple[list[Path], list[int], int, tuple[int, int]]:
    frames_dir.mkdir()
    paths: list[Path] = []
    durations: list[int] = []
    digests: set[str] = set()
    with Image.open(input_path) as image:
        frame_count = int(getattr(image, "n_frames", 1))
        size = image.size
        for index in range(frame_count):
            image.seek(index)
            rgba = image.convert("RGBA")
            path = frames_dir / f"frame-{index + 1:06}.png"
            rgba.save(path, format="PNG", compress_level=1)
            duration = int(image.info.get("duration") or default_duration_ms)
            if duration < 1:
                raise RuntimeError(f"frame {index + 1} has invalid duration {duration}")
            paths.append(path)
            durations.append(duration)
            digests.add(hashlib.sha256(rgba.tobytes()).hexdigest())
    return paths, durations, len(digests), size


def _encode_command(
    config: CodecConfig,
    source: Path,
    output: Path,
    args: argparse.Namespace,
    tools: dict[str, str],
) -> list[str]:
    if config.family == "webp":
        command = [tools["cwebp"], "-quiet"]
        if config.lossless:
            command.extend(["-lossless", "1"])
        command.extend(
            [
                "-q",
                f"{args.webp_quality:g}",
                "-alpha_q",
                "100",
                "-m",
                str(args.webp_method),
                str(source),
                "-o",
                str(output),
            ]
        )
        return command
    command = [tools["avifenc"]]
    if config.lossless:
        command.append("--lossless")
    else:
        command.extend(["-q", str(args.avif_quality), "--qalpha", "100"])
    command.extend(
        [
            "-s",
            str(args.avif_speed),
            "-j",
            args.avif_jobs,
            str(source),
            str(output),
        ]
    )
    return command


def _animation_command(
    config: CodecConfig,
    frames: list[Path],
    durations: list[int],
    output: Path,
    args: argparse.Namespace,
    tools: dict[str, str],
) -> list[str]:
    if config.family == "webp":
        command = [tools["img2webp"], "-loop", "0"]
        frame_options = [
            "-lossless" if config.lossless else "-lossy",
            "-q",
            f"{args.webp_quality:g}",
            "-m",
            str(args.webp_method),
        ]
        for frame, duration in zip(frames, durations, strict=True):
            command.extend([*frame_options, "-d", str(duration), str(frame)])
        command.extend(["-o", str(output)])
        return command
    command = [tools["avifenc"], "--timescale", "1000"]
    if config.lossless:
        command.append("--lossless")
    else:
        command.extend(["-q", str(args.avif_quality), "--qalpha", "100"])
    command.extend(["-s", str(args.avif_speed), "-j", args.avif_jobs])
    for frame, duration in zip(frames, durations, strict=True):
        command.extend(["--duration", str(duration), str(frame)])
    command.append(str(output))
    return command


def _composite_white(rgba: np.ndarray) -> np.ndarray:
    values = rgba.astype(np.float32)
    alpha = values[..., 3:4] / 255.0
    return values[..., :3] * alpha + 255.0 * (1.0 - alpha)


def _ssim_8x8(left: np.ndarray, right: np.ndarray) -> float:
    """Mean non-overlapping 8x8 RGB SSIM, dependency-free beyond NumPy."""
    height = left.shape[0] - left.shape[0] % 8
    width = left.shape[1] - left.shape[1] % 8

    def blocks(image: np.ndarray) -> np.ndarray:
        cropped = image[:height, :width]
        return cropped.reshape(height // 8, 8, width // 8, 8, 3).transpose(
            0, 2, 1, 3, 4
        )

    x = blocks(left)
    y = blocks(right)
    mean_x = x.mean(axis=(2, 3), keepdims=True)
    mean_y = y.mean(axis=(2, 3), keepdims=True)
    centered_x = x - mean_x
    centered_y = y - mean_y
    variance_x = np.mean(centered_x * centered_x, axis=(2, 3))
    variance_y = np.mean(centered_y * centered_y, axis=(2, 3))
    covariance = np.mean(centered_x * centered_y, axis=(2, 3))
    mean_x = mean_x[..., 0, 0, :]
    mean_y = mean_y[..., 0, 0, :]
    c1 = (0.01 * 255.0) ** 2
    c2 = (0.03 * 255.0) ** 2
    numerator = (2 * mean_x * mean_y + c1) * (2 * covariance + c2)
    denominator = (mean_x * mean_x + mean_y * mean_y + c1) * (
        variance_x + variance_y + c2
    )
    return float(np.mean(numerator / denominator))


def _decode_still(
    path: Path,
    family: str,
    decoded: Path,
    tools: dict[str, str],
) -> np.ndarray:
    if family == "webp":
        with Image.open(path) as image:
            return np.asarray(image.convert("RGBA"), dtype=np.uint8)
    _run([tools["avifdec"], str(path), str(decoded)])
    with Image.open(decoded) as image:
        return np.asarray(image.convert("RGBA"), dtype=np.uint8)


def _decode_animation_frame(
    path: Path,
    family: str,
    index: int,
    decoded: Path,
    tools: dict[str, str],
) -> np.ndarray:
    if family == "webp":
        with Image.open(path) as image:
            image.seek(index)
            return np.asarray(image.convert("RGBA"), dtype=np.uint8)
    _run([tools["avifdec"], "--index", str(index), str(path), str(decoded)])
    with Image.open(decoded) as image:
        return np.asarray(image.convert("RGBA"), dtype=np.uint8)


def quality_metrics(
    source_paths: list[Path],
    encoded_paths: list[Path],
    family: str,
    decoded_dir: Path,
    tools: dict[str, str],
    *,
    animation: bool,
    indices: list[int],
) -> dict[str, Any]:
    decoded_dir.mkdir(parents=True, exist_ok=True)
    squared_error = 0.0
    absolute_error = 0.0
    value_count = 0
    ssim_values: list[float] = []
    alpha_exact = True
    rgba_exact = True
    for position, (source_path, index) in enumerate(
        zip(source_paths, indices, strict=True)
    ):
        with Image.open(source_path) as source_image:
            source = np.asarray(source_image.convert("RGBA"), dtype=np.uint8)
        decoded_path = decoded_dir / f"frame-{index + 1:06}.png"
        if animation:
            decoded = _decode_animation_frame(
                encoded_paths[0], family, index, decoded_path, tools
            )
        else:
            decoded = _decode_still(
                encoded_paths[position], family, decoded_path, tools
            )
        if decoded.shape != source.shape:
            raise RuntimeError(
                f"decoded shape mismatch at frame {index + 1}: {decoded.shape} != {source.shape}"
            )
        alpha_exact = alpha_exact and np.array_equal(source[..., 3], decoded[..., 3])
        rgba_exact = rgba_exact and np.array_equal(source, decoded)
        source_display = _composite_white(source)
        decoded_display = _composite_white(decoded)
        difference = source_display - decoded_display
        squared_error += float(np.sum(difference * difference))
        absolute_error += float(np.sum(np.abs(difference)))
        value_count += difference.size
        ssim_values.append(_ssim_8x8(source_display, decoded_display))
    mse = squared_error / value_count
    return {
        "psnr_db": None if mse == 0 else 10.0 * math.log10(255.0**2 / mse),
        "ssim_8x8": sum(ssim_values) / len(ssim_values),
        "mae": absolute_error / value_count,
        "alpha_exact": alpha_exact,
        "rgba_exact": rgba_exact,
    }


def _result(
    config: CodecConfig,
    scope: str,
    frame_count: int,
    seconds: float,
    output_paths: list[Path],
    metrics: dict[str, Any],
    size: tuple[int, int],
) -> BenchmarkResult:
    byte_count = sum(path.stat().st_size for path in output_paths)
    megapixels = size[0] * size[1] * frame_count / 1_000_000
    return BenchmarkResult(
        name=config.name,
        scope=scope,
        frames=frame_count,
        seconds=round(seconds, 4),
        bytes=byte_count,
        bytes_per_frame=round(byte_count / frame_count, 2),
        megapixels_per_second=round(megapixels / seconds, 3),
        psnr_db=None if metrics["psnr_db"] is None else round(metrics["psnr_db"], 4),
        ssim_8x8=round(metrics["ssim_8x8"], 7),
        mae=round(metrics["mae"], 5),
        alpha_exact=metrics["alpha_exact"],
        rgba_exact=metrics["rgba_exact"],
        output=str(
            output_paths[0].parent if len(output_paths) > 1 else output_paths[0]
        ),
    )


def benchmark_samples(
    frames: list[Path],
    indices: list[int],
    output_dir: Path,
    size: tuple[int, int],
    args: argparse.Namespace,
    tools: dict[str, str],
) -> list[BenchmarkResult]:
    sources = [frames[index] for index in indices]
    results: list[BenchmarkResult] = []
    for config in CONFIGS:
        encoded_dir = output_dir / "sample" / config.name
        encoded_dir.mkdir(parents=True)
        outputs = [
            encoded_dir / f"frame-{index + 1:06}{config.extension}" for index in indices
        ]
        print(
            f"Encoding {len(indices)} independent frames as {config.name}...",
            flush=True,
        )
        started = time.perf_counter()
        for source, output in zip(sources, outputs, strict=True):
            _run(_encode_command(config, source, output, args, tools))
        seconds = time.perf_counter() - started
        metrics = quality_metrics(
            sources,
            outputs,
            config.family,
            output_dir / "decoded-sample" / config.name,
            tools,
            animation=False,
            indices=indices,
        )
        results.append(
            _result(
                config, "independent", len(indices), seconds, outputs, metrics, size
            )
        )
    return results


def benchmark_animations(
    frames: list[Path],
    durations: list[int],
    indices: list[int],
    output_dir: Path,
    size: tuple[int, int],
    args: argparse.Namespace,
    tools: dict[str, str],
) -> list[BenchmarkResult]:
    results: list[BenchmarkResult] = []
    sample_sources = [frames[index] for index in indices]
    for config in CONFIGS:
        output = output_dir / "animation" / f"animation-{config.name}{config.extension}"
        output.parent.mkdir(exist_ok=True)
        print(
            f"Encoding all {len(frames)} frames as animated {config.name}...",
            flush=True,
        )
        seconds = _timed(
            _animation_command(config, frames, durations, output, args, tools)
        )
        metrics = quality_metrics(
            sample_sources,
            [output],
            config.family,
            output_dir / "decoded-animation" / config.name,
            tools,
            animation=True,
            indices=indices,
        )
        results.append(
            _result(config, "animation", len(frames), seconds, [output], metrics, size)
        )
    return results


def _display_metric(value: float | None, digits: int) -> str:
    return "lossless" if value is None else f"{value:.{digits}f}"


def print_table(results: list[BenchmarkResult]) -> None:
    print(
        "\n| scope | codec | frames | seconds | MiB | KiB/frame | MP/s | PSNR dB | SSIM |"
    )
    print("| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |")
    for result in results:
        print(
            f"| {result.scope} | {result.name} | {result.frames} | {result.seconds:.2f} | "
            f"{result.bytes / 1024**2:.2f} | {result.bytes_per_frame / 1024:.1f} | "
            f"{result.megapixels_per_second:.2f} | {_display_metric(result.psnr_db, 2)} | "
            f"{_display_metric(result.ssim_8x8, 6)} |"
        )


def main() -> int:
    args = parser().parse_args()
    input_path = args.input.expanduser().resolve()
    if not input_path.is_file():
        raise SystemExit(f"input does not exist: {input_path}")
    if args.default_duration_ms < 1:
        raise SystemExit("--default-duration-ms must be at least 1")
    if not math.isfinite(args.webp_quality) or not 0 <= args.webp_quality <= 100:
        raise SystemExit("--webp-quality must be between 0 and 100")
    if not 0 <= args.avif_quality <= 100:
        raise SystemExit("--avif-quality must be between 0 and 100")
    if not 0 <= args.avif_speed <= 10:
        raise SystemExit("--avif-speed must be between 0 and 10")
    if not features.check("webp"):
        raise SystemExit("this Pillow build lacks WebP support")
    tools = {
        name: _tool(name)
        for name in (
            "cwebp",
            "avifenc",
            "avifdec",
            *(("img2webp",) if args.full_animation else ()),
        )
    }
    output_dir = _output_directory(input_path, args.output_dir)
    print(f"Work directory: {output_dir}", flush=True)
    print("Extracting lossless RGBA frames...", flush=True)
    extract_started = time.perf_counter()
    frames, durations, unique_frames, size = extract_frames(
        input_path, output_dir / "frames", args.default_duration_ms
    )
    extract_seconds = time.perf_counter() - extract_started
    indices = _sample_indices(len(frames), args.sample_frames)
    print(
        f"Source: {size[0]}x{size[1]}, {len(frames)} frames, {unique_frames} unique, "
        f"{sum(durations)} ms; extraction {extract_seconds:.2f}s",
        flush=True,
    )
    results = benchmark_samples(frames, indices, output_dir, size, args, tools)
    if args.full_animation:
        results.extend(
            benchmark_animations(
                frames, durations, indices, output_dir, size, args, tools
            )
        )
    report = {
        "source": {
            "path": str(input_path),
            "bytes": input_path.stat().st_size,
            "width": size[0],
            "height": size[1],
            "frames": len(frames),
            "unique_rgba_frames": unique_frames,
            "duration_ms": sum(durations),
            "extraction_seconds": round(extract_seconds, 4),
            "sample_indices_zero_based": indices,
        },
        "settings": {
            "webp_quality": args.webp_quality,
            "webp_method": args.webp_method,
            "avif_quality": args.avif_quality,
            "avif_speed": args.avif_speed,
            "avif_jobs": args.avif_jobs,
        },
        "versions": {
            "cwebp": _version([tools["cwebp"], "-version"]),
            "avifenc": _version([tools["avifenc"], "--version"]),
        },
        "results": [asdict(result) for result in results],
    }
    report_path = output_dir / "results.json"
    report_path.write_text(json.dumps(report, indent=2) + "\n")
    print_table(results)
    print(f"\nJSON report: {report_path}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
