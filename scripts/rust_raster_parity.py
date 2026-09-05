"""Exact output parity between the Python component-raster worker and the Rust
resvg-library worker.

Builds synthetic FFDec-style jobs (and reuses a real FFDec export when
available) in two filesystem stores, runs the Python reference
(``component_raster.rasterize_component_state`` with the resvg 0.48.1 CLI and
Pillow downsampling) in one and the Rust ``local-raster`` worker in the
other, then compares result records and decoded PNG pixels.

Usage:
    uv run --package aqw-char-renderer python scripts/rust_raster_parity.py
        [--resvg PATH] [--rust-binary PATH] [--work-dir DIR] [--keep]
"""

from __future__ import annotations

import argparse
import json
import shutil
import subprocess
import sys
import tarfile
import tempfile
from pathlib import Path

import numpy as np
from PIL import Image

sys.path.insert(0, "services/renderer/src")

from aqw_char_renderer.config import RuntimeConfig
from aqw_char_renderer.stages.component_raster import rasterize_component_state
from aqw_char_renderer.storage import FilesystemObjectStore

RASTER = 1024
OUTPUT = 512


def write_bundle(source_frames: dict[str, str], destination: Path) -> None:
    """Write the FFDec state SVGs into the bundle tar layout."""
    with tarfile.open(destination, "w:gz") as archive:
        for member, body in source_frames.items():
            info = tarfile.TarInfo(member)
            data = body.encode("utf-8")
            info.size = len(data)
            archive.addfile(info, __import__("io").BytesIO(data))


def ffdec_frame_svg() -> str:
    """A zoom-2 FFDec export exercising strokes, tints, gradients, fonts, and
    a nested placement color transform."""
    return """<?xml version="1.0" encoding="UTF-8" standalone="no"?>
<svg xmlns:ffdec="https://www.free-decompiler.com/flash" xmlns:xlink="http://www.w3.org/1999/xlink" ffdec:objectType="frame" height="20.0px" width="40.0px" xmlns="http://www.w3.org/2000/svg">
  <g transform="matrix(2.0, 0.0, 0.0, 2.0, 12.0, 16.0)">
    <use ffdec:characterId="11" height="10" width="20" xlink:href="#sprite0"/>
    <use ffdec:characterId="12" ffdec:characterName="Chest" height="10" width="20" xlink:href="#sprite1"/>
    <use ffdec:characterId="13" height="10" width="20" xlink:href="#sprite2"/>
  </g>
  <defs>
    <g id="sprite0">
      <rect x="0" y="0" width="20" height="10" fill="#33aa88" opacity="0.7"/>
    </g>
    <g id="sprite1">
      <path d="M1 1 L19 1 L19 9 L1 9 Z" fill="url(#grad1)" stroke="#222222" stroke-width="1" stroke-linejoin="round"
            ffdec:has-small-stroke="true" ffdec:original-stroke-width="0.05"/>
    </g>
    <g id="sprite2">
      <use ffdec:characterId="14" height="6" width="12" xlink:href="#sprite3" transform="matrix(1.0,0.0,0.0,1.0,4.0,4.0)"/>
    </g>
    <g id="sprite3">
      <ellipse cx="6" cy="3" rx="6" ry="3" fill="#ff00aa"/>
    </g>
    <linearGradient id="grad1" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0" stop-color="#dd3344"/>
      <stop offset="1" stop-color="#2244dd"/>
    </linearGradient>
    <g id="font_Beech_T1">
      <path d="M0 0 L5 0 L5 4 L0 4 Z" fill="#001122"/>
    </g>
    <g id="sprite4">
      <use ffdec:characterId="15" height="4" width="5" xlink:href="#font_Beech_T1" transform="matrix(2.0, 0.0, 0.0, 2.0, 0.0, 0.0)"/>
      <use ffdec:characterId="15" height="4" width="5" xlink:href="#font_Beech_T1" transform="matrix(2.0, 0.0, 0.0, 2.0, 0.0, -3.0)"/>
    </g>
  </defs>
</svg>
"""


def manifest(
    task_count: int, *, zoom: float, raster_size: int, output_size: int
) -> dict:
    return {
        "schema_version": 1,
        "job_id": "raster-parity",
        "render_hash": "parity",
        "final_key": "renders/raster-parity.webp",
        "frame_count": task_count,
        "frame_rate": 25.0,
        "viewbox": [0.0, 0.0, 512.0, 512.0],
        "frame_durations": [42] * task_count,
        "fields": {"intColorBase": "16711680", "strGender": "M"},
        "aliases": {},
        "weapon_type": "Sword",
        "parts": {
            "armor": {
                "source_idx": 0,
                "root_class": "Armor",
                "character_id": 286,
                "frame_count": task_count,
                "root_timeline_frames": 1,
                "color_rules": {"chest": ["Base", "dark"]},
                "placement_colors": {
                    "286,12": {
                        "red_mult": 128,
                        "green_mult": 256,
                        "blue_mult": 200,
                        "alpha_mult": 256,
                        "red_add": -6,
                        "green_add": 0,
                        "blue_add": 12,
                        "alpha_add": 0,
                    }
                },
            }
        },
        "static_keys": [],
        "ground_animate": {},
        "all_color_rules": [["Base", "dark"], ["Hair", "light"]],
        "settings": {
            "facing": "right",
            "zoom": zoom,
            "raster_size": raster_size,
            "output_size": output_size,
            "padding": 0,
            "webp_quality": 85.0,
            "webp_method": 4,
        },
        "component_pipeline": True,
        "component_tasks": [
            {
                "task_id": f"task-{index}",
                "symbol_key": "armor",
                "layer_name": "chest",
                "layer_index": index,
                # identity + a rotation to exercise transformed bounds
                "matrix": [0.98, 0.2, -0.2, 0.98, 300.0, 140.0 + index * 10],
                "darken": index % 2 == 1,
                "bundle_key": "jobs/raster-parity/prepare/source-bundles/0.0.tar.gz",
                "member": "armor/000001.svg",
                "source_frame": 1,
                "state_signature": f"sig-{index}",
            }
            for index in range(task_count)
        ],
        "component_frames": [],
        "component_raster_space": "output",
        "component_batches": [],
    }


def build_synthetic_fixture() -> dict:
    """Build the synthetic FFDec job resources shared by both stores."""
    bundle_dir = tempfile.mkdtemp(prefix="aqw-raster-synth-")
    bundle_path = Path(bundle_dir) / "bundle.tar.gz"
    write_bundle({"armor/000001.svg": ffdec_frame_svg()}, bundle_path)
    bundle_bytes = bundle_path.read_bytes()
    shutil.rmtree(bundle_dir, ignore_errors=True)
    return {
        "manifest": manifest(3, zoom=2.0, raster_size=RASTER, output_size=OUTPUT),
        "bundle_bytes": bundle_bytes,
        "extra_1x": manifest(1, zoom=2.0, raster_size=RASTER, output_size=RASTER),
    }


def build_real_fixture() -> dict | None:
    """Use the real FFDec pet export when it is present locally."""
    real = [
        Path("/private/tmp/ffdec-samples/pet/000020.svg"),
        Path("/private/tmp/alina-armor-hand.tar.gz"),
    ]
    candidate = next((path for path in real if path.is_file()), None)
    if candidate is None:
        return None
    bundle_bytes = candidate.read_bytes()
    if candidate.suffix == ".tar.gz":
        # member names inside the existing bundle
        with tarfile.open(candidate) as archive:
            member = archive.getnames()[0]
    else:
        import io

        buffer = io.BytesIO()
        with tarfile.open(fileobj=buffer, mode="w:gz") as archive:
            archive.add(candidate, arcname="armor/000001.svg")
        bundle_bytes = buffer.getvalue()
        member = "armor/000001.svg"
    viewbox = [0.0, 0.0, 600.0, 600.0]
    manifest_data = manifest(1, zoom=1.0, raster_size=1024, output_size=512)
    manifest_data["viewbox"] = viewbox
    manifest_data["component_tasks"] = [
        {
            "task_id": "real-pet",
            "symbol_key": "armor",
            "layer_name": "pet",
            "layer_index": 8,
            "matrix": [1.0, 0.0, 0.0, 1.0, 0.0, 0.0],
            "darken": False,
            "bundle_key": "jobs/raster-parity/prepare/source-bundles/0.0.tar.gz",
            "member": member,
            "source_frame": 1,
            "state_signature": "real-pet-sig",
        }
    ]
    return {
        "manifest": manifest_data,
        "bundle_bytes": bundle_bytes,
        "extra_1x": None,
    }


def populate_store(root: Path, resources: dict) -> None:
    """Mirror the FilesystemObjectStore layout under ``root/work``."""
    job = resources["manifest"]["job_id"]
    manifest_path = root / "work" / "jobs" / job / "prepare" / "manifest.json"
    manifest_path.parent.mkdir(parents=True, exist_ok=True)
    manifest_path.write_text(
        json.dumps(resources["manifest"], sort_keys=True), encoding="utf-8"
    )
    bundle_path = (
        root / "work" / "jobs" / job / "prepare" / "source-bundles" / "0.0.tar.gz"
    )
    bundle_path.parent.mkdir(parents=True, exist_ok=True)
    bundle_path.write_bytes(resources["bundle_bytes"])


def run_python(root: Path, resvg: str, task_index: int) -> dict:
    store = FilesystemObjectStore(root)
    job = "raster-parity"
    config = RuntimeConfig(
        source_bucket="source",
        work_bucket="work",
        job_table="jobs",
        result_queue_url="https://sqs.example/results",
        public_base_url="https://chars.example.com",
        asset_dataset_version="dev-v1",
        asset_manifest_key="datasets/dev-v1/manifest.json",
        character_renderer_key="character-renderer/dev-v1/characterB.swf",
        finalizer_download_concurrency=4,
        component_raster_frame_cap=120,
        render_cache_enabled=False,
        rsvg_convert=resvg,
        cwebp=shutil.which("cwebp") or "cwebp",
        webpmux=shutil.which("webpmux") or "webpmux",
    )
    return rasterize_component_state(
        job_id=job,
        manifest_key=f"jobs/{job}/prepare/manifest.json",
        task_index=task_index,
        store=store,
        config=config,
    )


def run_rust(
    root: Path,
    binary: str,
    task_index: int,
    *,
    env: dict[str, str] | None = None,
    raster_backend: str = "resvg",
) -> dict:
    result = subprocess.run(
        [
            binary,
            "local-raster",
            "--store-root",
            str(root),
            "--job-id",
            "raster-parity",
            "--task-index",
            str(task_index),
            "--raster-backend",
            raster_backend,
        ],
        capture_output=True,
        text=True,
        check=False,
        env=env,
    )
    if result.returncode:
        raise SystemExit(f"rust local-raster failed:\n{result.stderr}")
    return json.loads(result.stdout)


def png_diff(root: Path, result: dict) -> tuple[int, int] | None:
    if result["empty"]:
        return None
    path = root / "work" / result["png_key"]
    with Image.open(path) as image:
        pixels = np.asarray(image.convert("RGBA"), dtype=np.int16)
    return int(pixels.max()), pixels.size // 4


FIR_MAX_PREMULT_DIFF = 24


def fir_tolerance(case: dict, task_count: int) -> tuple[bool, dict[str, object]]:
    """Compare the (default) FIR outputs to Pillow on a premultiplied,
    gray-composited basis within a bounded tolerance.

    FIR uses a different integer premultiply path than Pillow, so per-channel
    straight-alpha values can differ by up to ~9/255 on real content. Assert
    a generous-but-bounded envelope (<=24/255 premultiplied on gray) rather
    than exact equality; bbox/shape equality is already asserted by
    ``compare_one``.
    """
    import numpy as np

    max_over_tasks = 0
    diffs_pct = 0.0
    count = 0
    for task_index in range(task_count):
        py = case["py_result"][task_index]
        rs = case["rs_result"][task_index]
        if py["empty"] or rs["empty"]:
            continue
        with (
            Image.open(case["py_root"] / "work" / py["png_key"]) as a,
            Image.open(case["rs_root"] / "work" / rs["png_key"]) as b,
        ):
            a_img = a.convert("RGBA")
            b_img = b.convert("RGBA")

        def premult(im):
            arr = np.asarray(im, dtype=np.float32) / 255.0
            arr[..., :3] *= arr[..., 3:4]
            return arr

        # FIR may shift the alpha bbox by +-1 px. Paste each result onto a gray
        # page at its recorded (x, y) placement (exactly how the composer will
        # place them), then compare the aligned region. This mirrors a real
        # composite; bbox geometry drift itself is gated by compare_one.
        max_w = max(int(py["x"]) + int(py["width"]), int(rs["x"]) + int(rs["width"]))
        max_h = max(int(py["y"]) + int(py["height"]), int(rs["y"]) + int(rs["height"]))

        def page_on_gray(img, x, y, width=max_w, height=max_h):
            page = Image.new("RGBA", (width, height), (96, 96, 96, 255))
            page.alpha_composite(img, (x, y))
            return premult(page)

        a_arr = page_on_gray(a_img, int(py["x"]), int(py["y"]))
        b_arr = page_on_gray(b_img, int(rs["x"]), int(rs["y"]))
        diff = np.abs(a_arr - b_arr).max(axis=2)
        maxd = int(np.ceil(diff.max() * 255))
        max_over_tasks = max(max_over_tasks, maxd)
        diffs_pct += 100.0 * (diff > 0.02).mean()
        count += 1
    mean_sig = diffs_pct / max(1, count)
    ok = max_over_tasks <= FIR_MAX_PREMULT_DIFF and mean_sig < 0.1
    return ok, {
        "max_premultiplied_diff_255": max_over_tasks,
        "mean_pct_over_0_02": round(mean_sig, 4),
        "limit_255": FIR_MAX_PREMULT_DIFF,
    }


def compare_one(
    case: dict, task_index: int, *, strict: bool = False, backend: str = "resvg"
) -> dict:
    """Compare one task. With ``strict`` (the exact downsample path) everything
    must match Pillow exactly. With ``strict=False`` (the FIR default) bbox
    geometry may drift by +-1 px (measured on the synthetic fixture) because
    FIR rounds premultiply differently; pixel fidelity is enforced separately
    by ``fir_tolerance``. With ``backend='thorvg'`` the Rust side renders with
    ThorVG 1.1.1: the output is NOT pixel-identical to resvg, so the
    comparison is informational (reports the max channel diff and the share of
    pixels beyond a soft threshold) and only geometry consistency is asserted.
    """
    py = case["py_result"][task_index]
    rs = case["rs_result"][task_index]

    def bbox_ok():
        if strict:
            return (
                py["x"] == rs["x"],
                py["y"] == rs["y"],
                py["width"] == rs["width"],
                py["height"] == rs["height"],
            )
        return (
            abs(py["x"] - rs["x"]) <= 1,
            abs(py["y"] - rs["y"]) <= 1,
            abs(py["width"] - rs["width"]) <= 1,
            abs(py["height"] - rs["height"]) <= 1,
        )

    bx, by, bw, bh = bbox_ok()
    checks = {
        "empty": py["empty"] == rs["empty"],
        "x": bx,
        "y": by,
        "width": bw,
        "height": bh,
        "task_id": py["task_id"] == rs["task_id"],
        "component_raster_space": py["component_raster_space"]
        == rs["component_raster_space"],
        "canvas_width": py["canvas_width"] == rs["canvas_width"],
        "canvas_height": py["canvas_height"] == rs["canvas_height"],
    }
    if py["empty"] or rs["empty"]:
        pixel_status = (
            "both-empty" if (py["empty"] and rs["empty"]) else "MISMATCH-EMPTY"
        )
        max_diff = 0
    else:
        import numpy as np

        a_path = case["py_root"] / "work" / py["png_key"]
        b_path = case["rs_root"] / "work" / rs["png_key"]
        with Image.open(a_path) as a, Image.open(b_path) as b:
            a_arr = np.asarray(a.convert("RGBA"), dtype=np.int16)
            b_arr = np.asarray(b.convert("RGBA"), dtype=np.int16)
        if a_arr.shape != b_arr.shape:
            pixel_status = f"SHAPE {a_arr.shape} != {b_arr.shape}"
            max_diff = -1
        else:
            diff = int(np.abs(a_arr - b_arr).max())
            max_diff = diff
            if backend == "thorvg":
                significant = int((np.abs(a_arr - b_arr).max(axis=2) > 2).sum())
                total = a_arr.shape[0] * a_arr.shape[1]
                pixel_status = (
                    f"engines-differ max={diff} significant={(significant * 100) // max(total, 1)}% "
                    f"(informational)"
                )
            else:
                pixel_status = (
                    "exact"
                    if diff == 0
                    else (
                        f"diff={diff}" if strict else f"strict-fir-diff={diff} (tolerated)"
                    )
                )
    return {
        "task_index": task_index,
        "checks": checks,
        "pixel_status": pixel_status,
        "max_channel_diff": max_diff,
        "py": {k: py.get(k) for k in ("x", "y", "width", "height", "empty")},
        "rs": {k: rs.get(k) for k in ("x", "y", "width", "height", "empty")},
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--resvg", type=Path, default=Path("/tmp/resvg-src/target/release/resvg")
    )
    parser.add_argument(
        "--rust-binary",
        type=Path,
        default=Path(
            "services/component-raster-rust/target/release/aqw-component-raster"
        ),
    )
    parser.add_argument("--work-dir", type=Path)
    parser.add_argument("--keep", action="store_true")
    parser.add_argument(
        "--exact",
        action="store_true",
        help="run the Rust worker with AQW_DOWNSAMPLER=exact (expects bit-identical",
    )
    parser.add_argument(
        "--raster-backend",
        choices=("resvg", "thorvg"),
        default="resvg",
        help="SVG rasterizer for the Rust worker: resvg (default, parity) or thorvg "
        "(1.1.1; informational delta against the resvg reference)",
    )
    args = parser.parse_args()

    if not args.resvg.is_file():
        raise SystemExit(
            f"resvg CLI not found at {args.resvg}; build 0.48.1 locally first"
        )
    if not args.rust_binary.is_file():
        raise SystemExit(f"rust binary not found at {args.rust_binary}")

    temporary = tempfile.TemporaryDirectory(
        prefix="aqw-raster-parity-", delete=not args.keep
    )
    work = args.work_dir or Path(temporary.name)
    work.mkdir(parents=True, exist_ok=True)

    synthetic = build_synthetic_fixture()
    fixtures: list[tuple[str, dict]] = [("synthetic", synthetic)]
    if synthetic.get("extra_1x") is not None:
        fixtures.append(
            (
                "synthetic-1x",
                {
                    "manifest": synthetic["extra_1x"],
                    "bundle_bytes": synthetic["bundle_bytes"],
                },
            )
        )
    real = build_real_fixture()
    if real is not None:
        fixtures.append(("real-pet", real))
    else:
        print("note: real FFDec fixture unavailable; synthetic only", file=sys.stderr)

    summary: dict[str, object] = {}
    all_pass = True
    for name, resources in fixtures:
        py_root = work / f"{name}-py"
        rs_root = work / f"{name}-rs"
        populate_store(py_root, resources)
        populate_store(rs_root, resources)
        task_count = len(resources["manifest"]["component_tasks"])
        py_results = [
            run_python(py_root, str(args.resvg), index) for index in range(task_count)
        ]
        # The default (FIR) path is the deployed one: assert it stays within a
        # tight tolerance of Pillow. The exact resampler is separately proven
        # bit-identical by the unit test + the FIR-vs-exact dump harness.
        rs_results = []
        for index in range(task_count):
            env = None
            if args.exact:
                env = {"AQW_DOWNSAMPLER": "exact"}
            rs_results.append(
                run_rust(
                    rs_root,
                    str(args.rust_binary),
                    index,
                    env=env,
                    raster_backend=args.raster_backend,
                )
            )
        case = {
            "py_root": py_root,
            "rs_root": rs_root,
            "py_result": py_results,
            "rs_result": rs_results,
        }
        comparisons = [
            compare_one(case, index, strict=args.exact, backend=args.raster_backend)
            for index in range(task_count)
        ]
        if args.raster_backend == "thorvg":
            # ThorVG is an intentionally different renderer: no pixel-parity
            # claim. Assert that both engines produced a bbox of the same
            # shape (within the +-1 px tolerance) and report the visual delta.
            ok = all(all(compare["checks"].values()) for compare in comparisons)
            tolerance_ok, tolerance_stats = True, {}
        elif args.exact:
            # Pillow-verbatim path: exact geometry and bit-identical pixels.
            ok = all(
                compare["pixel_status"] in {"exact", "both-empty"}
                and all(compare["checks"].values())
                for compare in comparisons
            )
            tolerance_ok, tolerance_stats = True, {}
        else:
            # FIR default: geometry within +-1px (bbox flip risk) and the
            # premultiplied-on-gray tolerance gate.
            ok = all(all(compare["checks"].values()) for compare in comparisons)
            tolerance_ok, tolerance_stats = fir_tolerance(case, task_count)
        all_pass = all_pass and ok and tolerance_ok
        summary[name] = {
            "tasks": task_count,
            "ok": ok,
            "tolerance_ok": tolerance_ok,
            "tolerance": tolerance_stats,
            "comparisons": comparisons,
        }
        if not (ok and tolerance_ok):
            for compare in comparisons:
                if compare["pixel_status"] not in {"exact", "both-empty"} or not all(
                    compare["checks"].values()
                ):
                    print(json.dumps(compare, indent=2, sort_keys=True, default=str))

    print(
        json.dumps(
            {
                name: {"ok": value["ok"], "tolerance": value.get("tolerance")}
                for name, value in summary.items()
            }
        )
    )
    if all_pass:
        print("PASS")
        return 0
    print("FAIL")
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
