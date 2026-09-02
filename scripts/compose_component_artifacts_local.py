"""Run the chunked component compositor from downloaded production artifacts.

The input layout matches ``scripts/benchmark_frame_composition.py``:
``manifest.json``, ``component/results/*.json``, and
``component/rasters/*.png`` beneath one directory.
"""

from __future__ import annotations

import argparse
import json
import shutil
import tempfile
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
from time import perf_counter

import numpy as np
from aqw_char_renderer.batching import partition_frames
from aqw_char_renderer.config import RuntimeConfig
from aqw_char_renderer.stages.component_compose import compose_frame_batch
from aqw_char_renderer.stages.component_raster import component_workflow_result
from aqw_char_renderer.stages.finalize import finalize_job
from aqw_char_renderer.storage import FilesystemObjectStore
from PIL import Image


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser(description=__doc__)
    result.add_argument("--artifact-dir", type=Path, required=True)
    result.add_argument("--output", type=Path, required=True)
    result.add_argument("--frames-per-worker", type=int, default=10)
    result.add_argument("--workers", type=int, default=12)
    result.add_argument("--compositor", choices=("pillow", "pyvips"), default="pillow")
    result.add_argument("--compare-webp", type=Path)
    result.add_argument("--compare-frames", type=int, default=8)
    return result


def compare_animations(
    actual_path: Path,
    expected_path: Path,
    *,
    sample_count: int,
) -> dict[str, object]:
    with Image.open(actual_path) as actual, Image.open(expected_path) as expected:
        count = min(actual.n_frames, expected.n_frames)
        indices = sorted(
            {
                round(index * (count - 1) / max(1, sample_count - 1))
                for index in range(min(sample_count, count))
            }
        )
        differences: list[float] = []
        for index in indices:
            actual.seek(index)
            expected.seek(index)
            left = np.asarray(actual.convert("RGBA"), dtype=np.float32) / 255.0
            right = np.asarray(expected.convert("RGBA"), dtype=np.float32) / 255.0
            left[..., :3] *= left[..., 3:4]
            right[..., :3] *= right[..., 3:4]
            differences.append(float(np.abs(left - right).mean()))
        return {
            "expected_frames": expected.n_frames,
            "sample_frames": [index + 1 for index in indices],
            "premultiplied_rgba_mae": sum(differences) / len(differences),
        }


def main() -> int:
    args = parser().parse_args()
    manifest = json.loads((args.artifact_dir / "manifest.json").read_text(encoding="utf-8"))
    job_id = str(manifest["job_id"])
    frame_count = int(manifest["frame_count"])
    manifest_key = f"jobs/{job_id}/prepare/manifest.json"
    batches = [
        batch.to_dict()
        for batch in partition_frames(frame_count, args.frames_per_worker)
    ]
    manifest["component_batches"] = batches

    cwebp = shutil.which("cwebp")
    webpmux = shutil.which("webpmux")
    if cwebp is None or webpmux is None:
        raise SystemExit("cwebp and webpmux must be on PATH")

    records = [
        json.loads(path.read_text(encoding="utf-8"))
        for path in sorted((args.artifact_dir / "component" / "results").glob("*.json"))
    ]
    component_results = [component_workflow_result(record) for record in records]
    with tempfile.TemporaryDirectory(prefix="aqw-component-artifacts-") as temporary:
        root = Path(temporary)
        store = FilesystemObjectStore(root / "objects")
        store.write_json("work", manifest_key, manifest)
        for record in records:
            png_key = record.get("png_key")
            if record.get("empty") is True or not png_key:
                continue
            store.upload_file(
                args.artifact_dir
                / "component"
                / "rasters"
                / f"{record['task_id']}.png",
                "work",
                str(png_key),
            )

        config = RuntimeConfig(
            source_bucket="source",
            work_bucket="work",
            job_table="jobs",
            result_queue_url="https://sqs.example/results",
            public_base_url="https://chars.example.com",
            asset_dataset_version="dev-v1",
            asset_manifest_key="datasets/dev-v1/manifest.json",
            character_renderer_key="character-renderer/dev-v1/characterB.swf",
            finalizer_download_concurrency=32,
            component_raster_enabled=True,
            component_raster_frame_cap=frame_count,
            component_compose_frames_per_lambda=args.frames_per_worker,
            component_compositor=args.compositor,
            render_cache_enabled=False,
            cwebp=cwebp,
            webpmux=webpmux,
        )

        def compose(batch: dict[str, int]) -> dict[str, object]:
            return compose_frame_batch(
                job_id=job_id,
                manifest_key=manifest_key,
                component_results=component_results,
                batch=batch,
                store=store,
                config=config,
                compositor=args.compositor,
            )

        compose_started = perf_counter()
        with ThreadPoolExecutor(max_workers=args.workers) as executor:
            render_results = list(executor.map(compose, batches))
        compose_seconds = perf_counter() - compose_started

        finalize_started = perf_counter()
        final = finalize_job(
            job_id=job_id,
            manifest_key=manifest_key,
            render_results=render_results,
            store=store,
            config=config,
        )
        finalize_seconds = perf_counter() - finalize_started
        args.output.parent.mkdir(parents=True, exist_ok=True)
        store.download("work", final["final_key"], args.output)

    with Image.open(args.output) as animation:
        verification: dict[str, object] = {
            "frames": animation.n_frames,
            "width": animation.width,
            "height": animation.height,
            "bytes": args.output.stat().st_size,
        }
    if args.compare_webp is not None:
        verification.update(
            compare_animations(
                args.output,
                args.compare_webp,
                sample_count=args.compare_frames,
            )
        )
    print(
        json.dumps(
            {
                "component_count": len(component_results),
                "batch_count": len(batches),
                "frames_per_worker": args.frames_per_worker,
                "workers": args.workers,
                "compositor": args.compositor,
                "compose_map_seconds": compose_seconds,
                "finalize_seconds": finalize_seconds,
                "total_seconds": compose_seconds + finalize_seconds,
                "output": str(args.output),
                "verification": verification,
            },
            indent=2,
            sort_keys=True,
        )
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
