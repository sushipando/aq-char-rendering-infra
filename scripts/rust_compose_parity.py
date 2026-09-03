"""Pixel parity between Pillow and the Rust component-compose worker.

Generates a synthetic v19 output-grid job (``component_raster_space =
"output"``) with translucent layers, half-transparent overlap, order swaps,
negative and off-canvas placements, and one empty task; composes reference
frames with Pillow's ``Image.alpha_composite`` (the production compositor);
runs the Rust local mode; and compares lossless RGBA and the cwebp-encoded
WebP frame bytes.

The Rust compositor blends in premultiplied RGBA with SIMD source-over, so
it is not bit-identical to Pillow's straight-alpha kernel; the comparison is
tolerance-based (``--max-channel-diff``, default 2) and reports the
mismatch statistics.

Usage:
    uv run --package aqw-char-renderer python scripts/rust_compose_parity.py
        [--artifact-dir DIR] [--work-dir DIR] [--rust-binary PATH] [--frame-end N]
        [--max-channel-diff N]
"""

from __future__ import annotations

import argparse
import hashlib
import json
import shutil
import subprocess
import tempfile
from pathlib import Path

import numpy as np
from PIL import Image, ImageDraw

OUTPUT_SIZE = 256
RASTER_SIZE = 512
WEBP_QUALITY = 80.0
WEBP_METHOD = 4
# The premultiplied SIMD compositor is within a couple of units of Pillow's
# straight-alpha kernel; rounding differences compound slightly across
# stacked layers, so allow a small headroom. A broken kernel produces
# channel diffs of 50+, so this still catches real regressions.
DEFAULT_MAX_CHANNEL_DIFF = 4


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def component(name: str, width: int, height: int, seed: int) -> Image.Image:
    """One translucent test component with hard edges, gradients, and AA dots."""
    rng = np.random.default_rng(seed)
    image = Image.new("RGBA", (width, height), (0, 0, 0, 0))
    draw = ImageDraw.Draw(image)
    base = tuple(int(v) for v in rng.integers(0, 256, size=3))
    alpha = int(rng.integers(60, 200))
    draw.rounded_rectangle(
        (0, 0, width - 1, height - 1), radius=6, fill=base + (alpha,)
    )
    # A vertical alpha gradient band so semi-transparent math is exercised.
    band_left = width // 4
    band_right = width // 2
    for x in range(band_left, band_right):
        t = (x - band_left) / max(1, band_right - band_left)
        shade = tuple(int(v * (0.4 + 0.6 * t)) for v in base)
        draw.line((x, 0, x, height - 1), fill=shade + (int(alpha * (0.3 + 0.7 * t)),))
    # Opaque border: tests opaque-over-translucent blending.
    draw.rectangle((0, 0, width - 1, height - 1), outline=(255, 255, 255, 255), width=2)
    # Opaque polka dots: tests opaque-over-opaque.
    for cy in range(8, height, 16):
        for cx in range(8, width, 16):
            value = tuple(int(v) for v in rng.integers(0, 256, size=3))
            draw.ellipse((cx - 3, cy - 3, cx + 3, cy + 3), fill=value + (255,))
    return image


class Fixture:
    def __init__(self, root: Path) -> None:
        self.root = root
        self.rasters = root / "component" / "rasters"
        self.results = root / "component" / "results"
        self.rasters.mkdir(parents=True, exist_ok=True)
        self.results.mkdir(parents=True, exist_ok=True)
        self.components: dict[str, Image.Image] = {}
        self.records: dict[str, dict[str, object]] = {}

    def add(
        self,
        task_id: str,
        image: Image.Image,
        x: int,
        y: int,
        *,
        empty: bool = False,
    ) -> None:
        record: dict[str, object] = {
            "task_id": task_id,
            "empty": empty,
            "x": x,
            "y": y,
            "component_raster_space": "output",
        }
        if not empty:
            path = self.rasters / f"{task_id}.png"
            image.save(path, format="PNG")
            record.update(
                {
                    "png_key": f"jobs/fixture/component/rasters/{task_id}.png",
                    "sha256": sha256_file(path),
                    "width": image.width,
                    "height": image.height,
                    "canvas_width": OUTPUT_SIZE,
                    "canvas_height": OUTPUT_SIZE,
                }
            )
            self.components[task_id] = image
        (self.results / f"{task_id}.json").write_text(
            json.dumps(record, sort_keys=True), encoding="utf-8"
        )
        self.records[task_id] = record

    def frames(self) -> list[dict[str, object]]:
        return [
            {"number": 1, "layers": ["a", "b", "c"], "duration_ms": 42},
            {"number": 2, "layers": ["b", "a", "c"], "duration_ms": 42},
            {
                "number": 3,
                "layers": ["e", "empty_task", "a", "b", "c"],
                "duration_ms": 41,
            },
        ]

    def write_manifest(self) -> None:
        manifest = {
            "schema_version": 1,
            "job_id": "rust-parity-fixture",
            "render_hash": "parity-fixture",
            "final_key": "renders/rust-parity-fixture.webp",
            "frame_count": len(self.frames()),
            "frame_rate": 25.0,
            "viewbox": [0.0, 0.0, 512.0, 512.0],
            "frame_durations": [42, 42, 41],
            "fields": {},
            "aliases": {},
            "weapon_type": "Sword",
            "parts": {},
            "static_keys": [],
            "ground_animate": {},
            "all_color_rules": [],
            "settings": {
                "facing": "right",
                "zoom": 1.0,
                "raster_size": RASTER_SIZE,
                "output_size": OUTPUT_SIZE,
                "padding": 0,
                "webp_quality": WEBP_QUALITY,
                "webp_method": WEBP_METHOD,
            },
            "component_pipeline": True,
            "component_tasks": [],
            "component_frames": self.frames(),
            "component_raster_space": "output",
            "component_batches": [],
        }
        (self.root / "manifest.json").write_text(
            json.dumps(manifest, sort_keys=True), encoding="utf-8"
        )

    def compose_reference(self, output_dir: Path) -> list[dict[str, object]]:
        """Pillow (production compositor) reference frames."""
        output_dir.mkdir(parents=True, exist_ok=True)
        frames = []
        for frame in self.frames():
            number = int(frame["number"])
            for task_id in frame["layers"]:
                if (
                    task_id not in self.components
                    and self.records[task_id]["empty"] is not True
                ):
                    raise RuntimeError(f"reference fixture missing component {task_id}")
            canvas = Image.new("RGBA", (OUTPUT_SIZE, OUTPUT_SIZE), (0, 0, 0, 0))
            for task_id in frame["layers"]:
                record = self.records[task_id]
                if record["empty"] is True:
                    continue
                canvas.alpha_composite(
                    self.components[task_id],
                    dest=(int(record["x"]), int(record["y"])),
                )
            path = output_dir / f"frame-{number:06d}.png"
            canvas.save(path, format="PNG")
            frames.append((number, canvas))
        return frames

    def cwebp_encode(self, cwebp: str, png: Path, webp: Path) -> None:
        subprocess.run(
            [
                cwebp,
                "-quiet",
                "-q",
                f"{WEBP_QUALITY:g}",
                "-alpha_q",
                "100",
                "-m",
                str(WEBP_METHOD),
                str(png),
                "-o",
                str(webp),
            ],
            check=True,
            capture_output=True,
        )


def premultiplied_rgba_mae(left: Image.Image, right: Image.Image) -> float:
    a = np.asarray(left.convert("RGBA"), dtype=np.float32) / 255.0
    b = np.asarray(right.convert("RGBA"), dtype=np.float32) / 255.0
    a[..., :3] *= a[..., 3:4]
    b[..., :3] *= b[..., 3:4]
    return float(np.abs(a - b).mean())


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--artifact-dir", type=Path)
    parser.add_argument("--work-dir", type=Path)
    parser.add_argument(
        "--rust-binary",
        type=Path,
        default=Path(
            "services/component-compose-rust/target/release/aqw-component-compose"
        ),
    )
    parser.add_argument("--keep", action="store_true", help="keep generated fixtures")
    parser.add_argument(
        "--max-channel-diff",
        type=int,
        default=DEFAULT_MAX_CHANNEL_DIFF,
        help=f"maximum allowed per-channel difference vs Pillow (default {DEFAULT_MAX_CHANNEL_DIFF})",
    )
    args = parser.parse_args()

    cwebp = shutil.which("cwebp")
    if cwebp is None:
        raise SystemExit("cwebp must be on PATH for the WebP comparison")
    if not args.rust_binary.is_file():
        raise SystemExit(
            f"rust binary not found at {args.rust_binary}; build it with "
            "cargo build --release --manifest-path services/component-compose-rust/Cargo.toml"
        )

    temporary = tempfile.TemporaryDirectory(
        prefix="aqw-rust-parity-", delete=not args.keep
    )
    work_root = args.work_dir or Path(temporary.name)
    artifact_dir = args.artifact_dir or (work_root / "artifact")
    reference_dir = work_root / "reference"
    rust_dir = work_root / "rust"
    artifact_dir.mkdir(parents=True, exist_ok=True)
    reference_dir.mkdir(parents=True, exist_ok=True)
    rust_dir.mkdir(parents=True, exist_ok=True)

    fixture = Fixture(artifact_dir)
    fixture.add("a", component("a", 72, 60, seed=1), x=10, y=12)
    fixture.add("b", component("b", 48, 96, seed=2), x=230, y=140)
    fixture.add("c", component("c", 80, 80, seed=3), x=-30, y=215)
    fixture.add("empty_task", Image.new("RGBA", (1, 1)), x=0, y=0, empty=True)
    fixture.add("e", component("e", 256, 28, seed=4), x=0, y=0)
    fixture.write_manifest()

    reference_frames = fixture.compose_reference(reference_dir)
    for number, _canvas in reference_frames:
        fixture.cwebp_encode(
            cwebp,
            reference_dir / f"frame-{number:06d}.png",
            reference_dir / f"frame-{number:06d}.webp",
        )

    result = subprocess.run(
        [
            str(args.rust_binary.resolve()),
            "local-compose",
            "--artifact-dir",
            str(artifact_dir),
            "--output-dir",
            str(rust_dir),
            "--frame-start",
            "1",
            "--frame-end",
            str(len(fixture.frames())),
            "--cwebp",
            cwebp,
        ],
        capture_output=True,
        text=True,
        check=False,
    )
    if result.returncode:
        raise SystemExit(f"rust local-compose failed:\n{result.stderr}")

    mismatches = 0
    max_diff = 0
    mean_diff = 0.0
    webp_identical = True
    for number, _canvas in reference_frames:
        reference = np.asarray(
            Image.open(reference_dir / f"frame-{number:06d}.png"), dtype=np.int16
        )
        actual = np.asarray(
            Image.open(rust_dir / "frames" / f"{number:06d}.png"), dtype=np.int16
        )
        if reference.shape != actual.shape:
            raise SystemExit(
                f"frame {number} size mismatch: reference {reference.shape} rust {actual.shape}"
            )
        diff = int(np.abs(reference - actual).max())
        max_diff = max(max_diff, diff)
        mean_diff = max(mean_diff, float(np.abs(reference - actual).mean()))
        if not np.array_equal(reference, actual):
            mismatches += np.count_nonzero(np.any(reference != actual, axis=2))
        rust_webp = rust_dir / "frames" / f"{number:06d}.webp"
        reference_webp = reference_dir / f"frame-{number:06d}.webp"
        if rust_webp.read_bytes() != reference_webp.read_bytes():
            webp_identical = False

    batch = json.loads((rust_dir / "batch-0000.json").read_text(encoding="utf-8"))
    assert batch["schema_version"] == 1
    assert [frame["frame"] for frame in batch["frames"]] == [1, 2, 3]
    assert [frame["duration"] for frame in batch["frames"]] == [42, 42, 41]
    assert {frame["canvas_width"] for frame in batch["frames"]} == {OUTPUT_SIZE}
    assert {frame["canvas_height"] for frame in batch["frames"]} == {OUTPUT_SIZE}
    for frame in batch["frames"]:
        webp = rust_dir / "frames" / f"{int(frame['frame']):06d}.webp"
        assert frame["sha256"] == sha256_file(webp), "batch manifest sha256 mismatch"
        assert frame["bytes"] == webp.stat().st_size

    summary = {
        "frames_compared": len(reference_frames),
        "exact_rgba": bool(mismatches == 0),
        "mismatched_pixels": int(mismatches),
        "max_abs_channel_diff": int(max_diff),
        "mean_abs_channel_diff": round(float(mean_diff), 4),
        "webp_bytes_identical": bool(webp_identical),
        "batch_manifest_schema": "ok",
        "artifact_dir": str(artifact_dir),
        "reference_dir": str(reference_dir),
        "rust_dir": str(rust_dir),
    }
    if args.keep:
        print(json.dumps(summary, indent=2, sort_keys=True))
    else:
        print(
            json.dumps({k: v for k, v in summary.items() if isinstance(v, (bool, int, float))})
        )
    if max_diff > args.max_channel_diff:
        raise SystemExit(
            f"max channel diff {max_diff} exceeds tolerance {args.max_channel_diff}"
        )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
