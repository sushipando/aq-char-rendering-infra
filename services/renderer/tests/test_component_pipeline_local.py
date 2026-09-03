"""Local coverage of the component-raster benchmark stages.

These tests run the real rsvg-convert/cwebp/webpmux binaries against tiny
synthetic FFDec exports, mirroring test_render_pipeline_local.py: build a
component-pipeline manifest exactly like prepare_finish emits, rasterize the
unique placed component states, then compose all frames in one call.
"""

from __future__ import annotations

import shutil
import tarfile
import tempfile
from pathlib import Path
from typing import Any

import numpy as np
import pytest
from PIL import Image, ImageDraw

from aqw_char_renderer import character_svg
from aqw_char_renderer.config import RuntimeConfig
from aqw_char_renderer.hashing import file_sha256
from aqw_char_renderer.stages.component_compose import compose_frame_batch
from aqw_char_renderer.stages.component_raster import (
    _downsample_component_to_output_grid,
    component_workflow_result,
    rasterize_component_state,
)
from aqw_char_renderer.stages.compose_frames import _downsample_image, compose_all_frames
from aqw_char_renderer.stages.finalize import finalize_job
from aqw_char_renderer.stages.prepare import (
    build_component_manifest as build_placed_component_manifest,
)
from aqw_char_renderer.stages.prepare import (
    shared_viewbox,
)
from aqw_char_renderer.storage import FilesystemObjectStore

RSVG_CONVERT = shutil.which("rsvg-convert")
CWEBP = shutil.which("cwebp")
WEBPMUX = shutil.which("webpmux")
BINARIES_AVAILABLE = all([RSVG_CONVERT, CWEBP, WEBPMUX])

pytestmark = pytest.mark.skipif(
    not BINARIES_AVAILABLE,
    reason="rsvg-convert/cwebp/webpmux toolchain is not installed",
)

ZOOM = 2.0


def frame_svg(
    red: int,
    green: int = 0,
    *,
    visible: bool = True,
    opacity: float | None = None,
) -> str:
    rendered_opacity = str(opacity) if opacity is not None else ("1" if visible else "0")
    return f"""<?xml version="1.0" encoding="UTF-8" standalone="no"?>
<svg xmlns:ffdec="https://www.free-decompiler.com/flash" xmlns:xlink="http://www.w3.org/1999/xlink" ffdec:objectType="frame" height="20px" width="40px" xmlns="http://www.w3.org/2000/svg">
  <g transform="matrix({ZOOM}, 0.0, 0.0, {ZOOM}, 12.0, 16.0)">
    <use ffdec:characterId="11" height="10" width="20" xlink:href="#sprite0"/>
  </g>
  <defs>
    <g id="sprite0">
      <rect x="0" y="0" width="20" height="10" fill="#{red:02x}{green:02x}00" opacity="{rendered_opacity}"/>
    </g>
  </defs>
</svg>
"""


def write_frames(root: Path, bodies: list[str]) -> list[Path]:
    paths = []
    for index, body in enumerate(bodies, start=1):
        path = root / "frames" / f"{index:06d}.svg"
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(body, encoding="utf-8")
        paths.append(path)
    return paths


def write_source_bundle(root: Path, symbol_key: str, frames: list[Path]) -> Path:
    archive_path = root / f"{symbol_key}.tar.gz"
    with tarfile.open(archive_path, "w:gz") as archive:
        for index, path in enumerate(frames, start=1):
            archive.add(path, arcname=f"{symbol_key}/{index:06d}.svg")
    return archive_path


def pipeline_config() -> RuntimeConfig:
    return RuntimeConfig(
        source_bucket="source",
        work_bucket="work",
        job_table="jobs",
        result_queue_url="https://sqs.example/results",
        public_base_url="https://chars.example.com",
        asset_dataset_version="dev-v1",
        asset_manifest_key="datasets/dev-v1/manifest.json",
        character_renderer_key="character-renderer/dev-v1/characterB.swf",
        finalizer_download_concurrency=4,
        component_raster_enabled=True,
        component_raster_concurrency=4,
        component_raster_frame_cap=25,
        render_cache_enabled=False,
        rsvg_convert=str(RSVG_CONVERT),
        cwebp=str(CWEBP),
        webpmux=str(WEBPMUX),
    )


def store_component_manifest(
    store: FilesystemObjectStore,
    *,
    job_id: str,
    frame_paths: list[Path],
    bundle_key: str,
    raster_size: int = 512,
    output_size: int = 256,
    component_raster_space: str | None = "output",
) -> tuple[str, list[dict[str, Any]], list[dict[str, Any]]]:
    frame_count = len(frame_paths)
    signatures = [file_sha256(path) for path in frame_paths]
    parts: dict[str, dict[str, Any]] = {
        "armor": {
            "source_idx": 0,
            "root_class": "Armor",
            "character_id": 286,
            "frame_count": frame_count,
            "root_timeline_frames": 1,
            "color_rules": {},
            "placement_colors": {},
        }
    }
    layers = character_svg.build_layers({"chest": "armor"}, weapon_type="Sword")
    viewbox = shared_viewbox(
        layers,
        {"armor": frame_paths},
        frame_count=frame_count,
        facing="right",
        zoom=ZOOM,
        max_size=output_size,
        padding=0,
    )
    tasks, component_frames = build_placed_component_manifest(
        job_id=job_id,
        layers=layers,
        symbol_signatures={"armor": signatures},
        part_manifest=parts,
        frame_count=frame_count,
        frame_durations=[40] * frame_count,
        facing="right",
        weapon_type="Sword",
        viewbox=viewbox,
        raster_size=raster_size,
        output_size=output_size,
        fields={"strGender": "M", "intColorBase": "16711680"},
        static_keys=[],
        ground_animate={},
        detected_blink_frames=None,
        ignored_loop_keys=[],
        source_bundle_frame_count=4,
        renderer_version="v19",
    )
    manifest: dict[str, Any] = {
        "schema_version": 1,
        "job_id": job_id,
        "render_hash": "abc123",
        "final_key": f"renders/{job_id}.webp",
        "frame_count": frame_count,
        "frame_rate": 25.0,
        "viewbox": list(viewbox),
        "frame_durations": [40] * frame_count,
        "fields": {"strGender": "M", "intColorBase": "16711680"},
        "aliases": {"chest": "armor"},
        "weapon_type": "Sword",
        "parts": parts,
        "static_keys": [],
        "ground_animate": {},
        "all_color_rules": [["Base", "None"]],
        "settings": {
            "facing": "right",
            "zoom": ZOOM,
            "raster_size": raster_size,
            "output_size": output_size,
            "padding": 0,
            "webp_quality": 80,
            "webp_method": 4,
        },
        "component_pipeline": True,
        "component_tasks": tasks,
        "component_frames": component_frames,
    }
    if component_raster_space is not None:
        manifest["component_raster_space"] = component_raster_space
    manifest_key = f"jobs/{job_id}/prepare/manifest.json"
    store.write_json("work", manifest_key, manifest)
    store.upload_file(bundle_key, "work", f"jobs/{job_id}/prepare/source-bundles/0.0.tar.gz")
    return manifest_key, tasks, component_frames


def run_component_job(
    store: FilesystemObjectStore,
    *,
    job_id: str,
    manifest_key: str,
    tasks: list[dict[str, Any]],
    config: RuntimeConfig,
) -> list[dict[str, Any]]:
    results = [
        rasterize_component_state(
            job_id=job_id,
            manifest_key=manifest_key,
            task_index=task_index,
            store=store,
            config=config,
        )
        for task_index in range(len(tasks))
    ]
    assert len(results) == len(tasks)
    assert {result["task_id"] for result in results} == {task["task_id"] for task in tasks}
    return results


@pytest.mark.parametrize("compositor", ["pillow"])
def test_component_raster_then_compose_deduplicates_states(compositor: str) -> None:
    frame_count = 3
    job_id = "job-component"
    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary)
        store = FilesystemObjectStore(root / "objects")
        # Two byte-identical frames share one unique state: 3 output frames
        # must produce only 2 unique placed component tasks.
        frame_paths = write_frames(
            root,
            [frame_svg(red=120), frame_svg(red=120), frame_svg(red=200)],
        )
        bundle = write_source_bundle(root, "armor", frame_paths)
        config = pipeline_config()
        manifest_key, tasks, component_frames = store_component_manifest(
            store,
            job_id=job_id,
            frame_paths=frame_paths,
            bundle_key=bundle,
        )
        assert len(tasks) == 2
        assert [len(frame["layers"]) for frame in component_frames] == [1, 1, 1]

        results = run_component_job(
            store,
            job_id=job_id,
            manifest_key=manifest_key,
            tasks=tasks,
            config=config,
        )
        assert all(result.get("empty") is not True for result in results)
        for result in results:
            assert result["width"] > 0 and result["height"] > 0
            assert result["x"] >= 0 and result["y"] >= 0
            assert result["component_raster_space"] == "output"
            assert max(result["canvas_width"], result["canvas_height"]) == 256
        # The chest layer is translated inside the character; its exact
        # canvas offset must be recorded for integer-offset composition.
        offsets = {(result["x"], result["y"]) for result in results}
        assert len(offsets) == 1 and (0, 0) not in offsets

        compact_results = [component_workflow_result(result) for result in results]
        render_results = [
            compose_frame_batch(
                job_id=job_id,
                manifest_key=manifest_key,
                component_results=compact_results,
                batch=batch,
                store=store,
                config=config,
                compositor=compositor,
            )
            for batch in (
                {"index": 0, "frame_start": 1, "frame_end": 2},
                {"index": 1, "frame_start": 3, "frame_end": 3},
            )
        ]
        assert [result["batch"] for result in render_results] == [0, 1]
        composed = finalize_job(
            job_id=job_id,
            manifest_key=manifest_key,
            render_results=render_results,
            store=store,
            config=config,
        )
        assert composed["frame_count"] == frame_count
        assert composed["cache_hit"] is False
        output = root / "objects" / "work" / composed["final_key"]
        with Image.open(output) as animation:
            assert animation.format == "WEBP"
            assert animation.n_frames == frame_count
            assert animation.info.get("loop") == 0
            # Each 512 raster component was downsampled exactly once in its
            # component worker; composition already happened at output size.
            assert max(animation.size) == 256


def _premultiplied_rgba(image: Image.Image) -> np.ndarray:
    value = np.asarray(image.convert("RGBA"), dtype=np.int16)
    alpha = value[..., 3:4]
    rgb = (value[..., :3] * alpha + 127) // 255
    return np.concatenate((rgb, alpha), axis=2)


def test_output_grid_resize_matches_full_canvas_with_odd_dimension_and_thin_lines() -> None:
    """Tight-layer resizing keeps the global phase and high-res hairlines.

    The 697 -> 348 axis exercises the awkward aspect-ratio rounding found in
    real character renders. The one-raster-pixel strokes stand in for SVG
    strokes after the existing minimum-stroke calibration; this optimization
    must only move the subsequent PNG shrink, never thicken those strokes.
    """
    raster_canvas = (697, 1024)
    output_canvas = (348, 512)
    positions = [(83, 127), (-5, 341), (512, -9), (620, 880)]
    for x, y in positions:
        component = Image.new("RGBA", (211, 179), (0, 0, 0, 0))
        draw = ImageDraw.Draw(component)
        draw.ellipse(
            (3, 4, 207, 175),
            fill=(130, 30, 240, 91),
            outline=(0, 0, 0, 255),
            width=1,
        )
        draw.line((0, 178, 210, 0), fill=(255, 255, 255, 127), width=1)

        full_raster = Image.new("RGBA", raster_canvas, (0, 0, 0, 0))
        full_raster.alpha_composite(component, dest=(x, y))
        expected = _downsample_image(full_raster, output_size=512)
        optimized = _downsample_component_to_output_grid(
            component,
            x=x,
            y=y,
            raster_canvas=raster_canvas,
            output_canvas=output_canvas,
        )
        actual = Image.new("RGBA", output_canvas, (0, 0, 0, 0))
        if optimized is not None:
            layer, layer_x, layer_y = optimized
            actual.alpha_composite(layer, dest=(layer_x, layer_y))
            layer.close()

        np.testing.assert_array_equal(
            _premultiplied_rgba(actual),
            _premultiplied_rgba(expected),
        )
        component.close()
        full_raster.close()
        expected.close()
        actual.close()


def test_legacy_component_manifest_keeps_raster_grid_fallback() -> None:
    """An in-flight pre-v19 manifest remains composable during deployment."""
    job_id = "job-legacy-raster-space"
    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary)
        store = FilesystemObjectStore(root / "objects")
        frame_paths = write_frames(root, [frame_svg(red=120)])
        bundle = write_source_bundle(root, "armor", frame_paths)
        manifest_key, tasks, _ = store_component_manifest(
            store,
            job_id=job_id,
            frame_paths=frame_paths,
            bundle_key=bundle,
            component_raster_space=None,
        )
        config = pipeline_config()
        results = run_component_job(
            store,
            job_id=job_id,
            manifest_key=manifest_key,
            tasks=tasks,
            config=config,
        )
        assert results[0]["component_raster_space"] == "raster"
        assert max(results[0]["canvas_width"], results[0]["canvas_height"]) == 512

        composed = compose_all_frames(
            job_id=job_id,
            manifest_key=manifest_key,
            component_results=[component_workflow_result(result) for result in results],
            store=store,
            config=config,
        )
        with Image.open(root / "objects" / "work" / composed["final_key"]) as animation:
            assert max(animation.size) == 256


def test_component_pipeline_invisible_state_is_empty_and_skipped() -> None:
    job_id = "job-empty"
    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary)
        store = FilesystemObjectStore(root / "objects")
        frame_paths = write_frames(root, [frame_svg(red=200, visible=False)])
        bundle = write_source_bundle(root, "armor", frame_paths)
        manifest_key, tasks, _ = store_component_manifest(
            store,
            job_id=job_id,
            frame_paths=frame_paths,
            bundle_key=bundle,
        )
        config = pipeline_config()
        results = run_component_job(
            store,
            job_id=job_id,
            manifest_key=manifest_key,
            tasks=tasks,
            config=config,
        )
        assert results[0]["empty"] is True
        assert results[0]["png_key"] is None
        # An invisible layer must still produce a valid (transparent) animation.
        composed = compose_all_frames(
            job_id=job_id,
            manifest_key=manifest_key,
            component_results=[component_workflow_result(result) for result in results],
            store=store,
            config=config,
        )
        with Image.open(root / "objects" / "work" / composed["final_key"]) as animation:
            assert animation.n_frames == 1
            assert animation.convert("RGBA").getchannel("A").getbbox() is None


def test_component_pipeline_preserves_translucent_source_alpha() -> None:
    """A layer's alpha must be applied once, using normal source-over.

    Pillow's masked ``paste`` applies the source alpha as both image alpha and
    mask, turning authored 50% opacity into roughly 25%. This regression test
    uses a same-size raster/output canvas so no downsampling can hide it.
    """
    job_id = "job-translucent"
    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary)
        store = FilesystemObjectStore(root / "objects")
        frame_paths = write_frames(root, [frame_svg(red=200, opacity=0.5)])
        bundle = write_source_bundle(root, "armor", frame_paths)
        manifest_key, tasks, _ = store_component_manifest(
            store,
            job_id=job_id,
            frame_paths=frame_paths,
            bundle_key=bundle,
            raster_size=256,
            output_size=256,
        )
        config = pipeline_config()
        results = run_component_job(
            store,
            job_id=job_id,
            manifest_key=manifest_key,
            tasks=tasks,
            config=config,
        )
        composed = compose_all_frames(
            job_id=job_id,
            manifest_key=manifest_key,
            component_results=[component_workflow_result(result) for result in results],
            store=store,
            config=config,
        )
        with Image.open(root / "objects" / "work" / composed["final_key"]) as animation:
            alpha = animation.convert("RGBA").getchannel("A")
            assert 120 <= alpha.getextrema()[1] <= 135


def test_compose_all_rejects_missing_component_results() -> None:
    job_id = "job-missing"
    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary)
        store = FilesystemObjectStore(root / "objects")
        frame_paths = write_frames(root, [frame_svg(red=120)])
        bundle = write_source_bundle(root, "armor", frame_paths)
        manifest_key, tasks, _ = store_component_manifest(
            store,
            job_id=job_id,
            frame_paths=frame_paths,
            bundle_key=bundle,
        )
        # A failed component must fail the whole composition; the compositor
        # must never silently omit a failed layer.
        with pytest.raises(character_svg.CharacterSvgError, match="missing results"):
            compose_all_frames(
                job_id=job_id,
                manifest_key=manifest_key,
                component_results=[],
                store=store,
                config=pipeline_config(),
            )
        with pytest.raises(character_svg.CharacterSvgError, match="missing results"):
            compose_frame_batch(
                job_id=job_id,
                manifest_key=manifest_key,
                component_results=[],
                batch={"index": 0, "frame_start": 1, "frame_end": 1},
                store=store,
                config=pipeline_config(),
            )
        assert tasks  # ensure the test used a real task id


def test_component_task_ids_are_deterministic() -> None:
    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary)
        store = FilesystemObjectStore(root / "objects")
        frame_paths = write_frames(root, [frame_svg(red=120), frame_svg(red=200)])
        bundle = write_source_bundle(root, "armor", frame_paths)
        manifest_key, tasks, _ = store_component_manifest(
            store,
            job_id="job-dup",
            frame_paths=frame_paths,
            bundle_key=bundle,
        )
        _, rebuilt, _ = store_component_manifest(
            store,
            job_id="job-dup",
            frame_paths=frame_paths,
            bundle_key=bundle,
        )
        _, other_output_size, _ = store_component_manifest(
            store,
            job_id="job-dup",
            frame_paths=frame_paths,
            bundle_key=bundle,
            output_size=128,
        )
        # Deterministic task IDs across manifest rebuilds, so retries and
        # re-runs write to identical S3 keys. Output-grid rasters at different
        # delivered sizes must not share those IDs.
        assert [task["task_id"] for task in tasks] == [task["task_id"] for task in rebuilt]
        assert [task["task_id"] for task in tasks] != [
            task["task_id"] for task in other_output_size
        ]
        assert manifest_key
