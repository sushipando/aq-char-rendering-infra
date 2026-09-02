#!/usr/bin/env python3
"""Local end-to-end run of the component-raster benchmark pipeline.

Exercises the exact distributed stage functions (prepare_resolve ->
prepare_export_source Map -> prepare_finish -> component-raster Map ->
compose_all_frames) against a FilesystemObjectStore, using local FFDec,
rsvg-convert, cwebp, and webpmux. Verifies the composed frames against the
first ``--compare-frames`` frames of a downloaded ground-truth WebP.

Usage:
    uv run --package aqw-char-renderer python scripts/render_component_pipeline_local.py \
        --output-dir /tmp/alina-component
"""

from __future__ import annotations

import argparse
import json
import shutil
import tempfile
import time
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
from uuid import uuid4

import numpy as np
from aqw_char_renderer.config import RuntimeConfig
from aqw_char_renderer.contracts import (
    DiscordTarget,
    JobRequest,
    RenderSettings,
    utc_now,
)
from aqw_char_renderer.hashing import file_sha256
from aqw_char_renderer.stages.component_compose import compose_frame_batch
from aqw_char_renderer.stages.component_raster import (
    component_workflow_result,
    rasterize_component_state,
)
from aqw_char_renderer.stages.finalize import finalize_job
from aqw_char_renderer.stages.prepare import (
    prepare_export_source,
    prepare_finish,
    prepare_resolve,
)
from aqw_char_renderer.storage import FilesystemObjectStore
from PIL import Image

FFDEC_JAR = Path("/tmp/ffdec-local/ffdec-cli.jar")
ALINA_PREPARE_INPUT = Path("/private/tmp/alina-prepare-input.json")

ALINA_SWFS = {
    "classes/F/ccsephA.swf": Path("/tmp/alina-swfs/ccsephA.swf"),
    "items/helms/cmagicianHLocksHat.swf": Path(
        "/tmp/alina-swfs/cmagicianHLocksHat.swf"
    ),
    "items/swords/trojanwWSword.swf": Path("/tmp/alina-swfs/trojanwWSword.swf"),
}
CHARACTER_RENDERER = Path("/private/tmp/characterB.swf")


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser(description=__doc__)
    result.add_argument("--output-dir", type=Path, default=Path("/tmp/alina-component"))
    result.add_argument("--username", default="alina")
    result.add_argument("--max-frames", type=int, default=25)
    result.add_argument("--frame-cap", type=int, default=25)
    result.add_argument("--raster-size", type=int, default=512)
    result.add_argument("--output-size", type=int, default=256)
    result.add_argument("--zoom", type=float, default=1.0)
    result.add_argument("--concurrency", type=int, default=5)
    result.add_argument("--compose-frames-per-worker", type=int, default=10)
    result.add_argument("--compose-concurrency", type=int, default=12)
    result.add_argument("--bundle-frames", type=int, default=4)
    result.add_argument(
        "--compare-webp", type=Path, default=Path("/tmp/gt/alina120-256.webp")
    )
    result.add_argument("--compare-frames", type=int, default=25)
    result.add_argument("--keep-sources", action="store_true")
    return result


def build_source_catalog(store: FilesystemObjectStore, *, temporary: Path) -> None:
    """Seed the local source bucket with a usable dev-v1 catalog manifest."""
    assets: dict[str, dict[str, object]] = {}
    for remote, local in ALINA_SWFS.items():
        key = f"datasets/dev-v1/swf/{remote}"
        store.upload_file(local, "source", key)
        assets[remote] = {
            "key": key,
            "sha256": file_sha256(local),
            "size": local.stat().st_size,
        }

    character_key = "character-renderer/dev-v1/characterB.swf"
    store.upload_file(CHARACTER_RENDERER, "source", character_key)
    item_database = temporary / "item_db.json"
    item_database.write_text("{}\n", encoding="utf-8")
    store.upload_file(item_database, "source", "datasets/dev-v1/item_db.json")
    store.write_json(
        "source",
        "datasets/dev-v1/manifest.json",
        {
            "schema_version": 1,
            "dataset_version": "dev-v1",
            "assets": assets,
            "item_database": {
                "key": "datasets/dev-v1/item_db.json",
                "sha256": file_sha256(item_database),
                "size": item_database.stat().st_size,
            },
            "character_renderer": {
                "key": character_key,
                "sha256": file_sha256(CHARACTER_RENDERER),
                "size": CHARACTER_RENDERER.stat().st_size,
            },
        },
    )


def compare_webp(
    actual: Path,
    expected: Path,
    *,
    frames: int,
    output_dir: Path,
) -> dict[str, float]:
    """Compare composed frames to the ground-truth animation's first frames."""
    with Image.open(actual) as rendered:
        assert rendered.format == "WEBP"
        actual_frames = [
            rendered.seek(frame) or rendered.convert("RGBA").copy()
            for frame in range(rendered.n_frames)
        ]
    with Image.open(expected) as truth:
        assert truth.format == "WEBP"
        truth_frames = [
            truth.seek(frame) or truth.convert("RGBA").copy()
            for frame in range(min(truth.n_frames, frames))
        ]
    summary: dict[str, float] = {"frame_count": float(len(actual_frames))}
    diffs: list[float] = []
    for index, (actual_image, truth_image) in enumerate(
        zip(actual_frames, truth_frames[: len(actual_frames)], strict=False)
    ):
        common = (
            min(actual_image.width, truth_image.width),
            min(actual_image.height, truth_image.height),
        )
        a = actual_image.convert("RGBA").resize(common)
        t = truth_image.convert("RGBA").resize(common)
        actual_data = np.asarray(a, dtype=np.int16)
        truth_data = np.asarray(t, dtype=np.int16)
        mean = float(np.abs(actual_data - truth_data).mean())
        diffs.append(mean)
        actual_image.convert("RGB").save(output_dir / f"frame-{index + 1:03d}.png")
    summary["mean_channel_error_frame1"] = diffs[0] if diffs else -1.0
    summary["mean_channel_error_avg"] = sum(diffs) / len(diffs) if diffs else -1.0
    return summary


def main() -> int:
    args = parser().parse_args()
    args.output_dir.mkdir(parents=True, exist_ok=True)
    prepared_input = json.loads(ALINA_PREPARE_INPUT.read_text(encoding="utf-8"))
    fields = {str(key): str(value) for key, value in prepared_input["fields"].items()}

    rsvg = shutil.which("rsvg-convert")
    cwebp = shutil.which("cwebp")
    webpmux = shutil.which("webpmux")
    if rsvg is None or cwebp is None or webpmux is None:
        raise SystemExit("rsvg-convert, cwebp, and webpmux must be on PATH")

    with tempfile.TemporaryDirectory(prefix="aqw-alina-component-") as temporary:
        root = Path(temporary)
        store = FilesystemObjectStore(root / "objects")
        build_source_catalog(store, temporary=root)
        config = RuntimeConfig(
            source_bucket="source",
            work_bucket="work",
            job_table="jobs",
            result_queue_url="https://sqs.example/results",
            public_base_url="https://chars.example.com",
            asset_dataset_version="dev-v1",
            asset_manifest_key="datasets/dev-v1/manifest.json",
            character_renderer_key="character-renderer/dev-v1/characterB.swf",
            renderer_version="v19",
            frames_per_render_lambda=1,
            source_bundle_frame_count=args.bundle_frames,
            finalizer_download_concurrency=8,
            render_cache_enabled=False,
            component_raster_enabled=True,
            component_raster_concurrency=args.concurrency,
            component_raster_frame_cap=args.frame_cap,
            component_compose_frames_per_lambda=args.compose_frames_per_worker,
            ffdec_path=FFDEC_JAR,
            rsvg_convert=rsvg,
            cwebp=cwebp,
            webpmux=webpmux,
        )
        request = JobRequest(
            job_id=str(uuid4()),
            created_at=utc_now(),
            discord=DiscordTarget(
                user_id="900000000000000001",
                channel_id="900000000000000002",
                guild_id="900000000000000003",
            ),
            render=RenderSettings(
                username=args.username,
                max_frames=args.max_frames,
                complete_loop=True,
                facing="right",
                zoom=args.zoom,
                raster_size=args.raster_size,
                output_size=args.output_size,
                padding=0,
                webp_quality=85,
                webp_method=4,
            ),
            appearance=fields,
        )

        phase = time.perf_counter()
        resolved = prepare_resolve(
            request, store=store, config=config, flashvars=fields
        )
        print(
            json.dumps(
                {
                    "event": "resolve",
                    "cache_hit": resolved["cache_hit"],
                    "sources": len(resolved.get("sources") or []),
                }
            )
        )
        export_results = []
        for source in resolved["sources"]:
            started = time.perf_counter()
            result = prepare_export_source(
                job_id=request.job_id,
                input_key=resolved["input_key"],
                source=source,
                store=store,
                config=config,
            )
            print(
                json.dumps(
                    {
                        "event": "export",
                        "source_idx": result["source_idx"],
                        "parts": len(result["parts"]),
                        "vector_cache_hit": result["vector_cache_hit"],
                        "seconds": round(time.perf_counter() - started, 1),
                    }
                )
            )
            export_results.append(result)

        finished = prepare_finish(
            request=request,
            input_key=resolved["input_key"],
            export_results=export_results,
            store=store,
            config=config,
        )
        if finished.get("component_pipeline") is not True:
            raise RuntimeError("PrepareFinish did not select the component pipeline")
        component_manifest = store.read_json("work", finished["manifest_key"])
        tasks = component_manifest["component_tasks"]
        workflow_state = {
            "request": request.to_dict(),
            "prepare": finished,
        }
        print(
            json.dumps(
                {
                    "event": "finish",
                    "frame_count": finished["frame_count"],
                    "task_count": len(tasks),
                    "state_payload_bytes": len(
                        json.dumps(workflow_state, separators=(",", ":")).encode()
                    ),
                    "seconds": round(time.perf_counter() - phase, 1),
                }
            )
        )

        def raster(task_index: int) -> dict[str, object]:
            return rasterize_component_state(
                job_id=request.job_id,
                manifest_key=finished["manifest_key"],
                task_index=task_index,
                store=store,
                config=config,
            )

        raster_started = time.perf_counter()
        with ThreadPoolExecutor(max_workers=args.concurrency) as pool:
            diagnostic_results = list(pool.map(raster, range(len(tasks))))
        component_results = [
            component_workflow_result(result) for result in diagnostic_results
        ]
        empty = sum(1 for result in diagnostic_results if result.get("empty"))
        print(
            json.dumps(
                {
                    "event": "component_map",
                    "completed": len(component_results),
                    "empty": empty,
                    "state_payload_bytes": len(
                        json.dumps(
                            {**workflow_state, "component_results": component_results},
                            separators=(",", ":"),
                        ).encode()
                    ),
                    "seconds": round(time.perf_counter() - raster_started, 1),
                }
            )
        )

        def compose(batch: dict[str, int]) -> dict[str, object]:
            return compose_frame_batch(
                job_id=request.job_id,
                manifest_key=finished["manifest_key"],
                component_results=component_results,
                batch=batch,
                store=store,
                config=config,
            )

        compose_started = time.perf_counter()
        component_batches = finished["component_batches"]
        with ThreadPoolExecutor(max_workers=args.compose_concurrency) as pool:
            render_results = list(pool.map(compose, component_batches))
        compose_seconds = time.perf_counter() - compose_started
        print(
            json.dumps(
                {
                    "event": "component_compose_map",
                    "batches": len(render_results),
                    "seconds": round(compose_seconds, 1),
                }
            )
        )
        finalize_started = time.perf_counter()
        composed = finalize_job(
            job_id=request.job_id,
            manifest_key=finished["manifest_key"],
            render_results=render_results,
            store=store,
            config=config,
        )
        print(
            json.dumps(
                {
                    "event": "finalize",
                    **composed,
                    "seconds": round(time.perf_counter() - finalize_started, 1),
                }
            )
        )

        output = args.output_dir / f"alina-component-{args.output_size}.webp"
        store.download("work", composed["final_key"], output)
        comparison: dict[str, float] = {}
        if args.compare_webp.is_file():
            comparison = compare_webp(
                output,
                args.compare_webp,
                frames=args.compare_frames,
                output_dir=args.output_dir,
            )
        print(json.dumps({"event": "verified", **comparison, "output": str(output)}))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
