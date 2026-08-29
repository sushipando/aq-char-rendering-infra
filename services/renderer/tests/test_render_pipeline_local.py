"""Local end-to-end coverage of the merged render stage and finalize.

These tests run the real rsvg-convert/cwebp/webpmux binaries against tiny
synthetic FFDec exports; they are skipped on machines without the toolchain.
"""

from __future__ import annotations

import shutil
import tarfile
import tempfile
from pathlib import Path
from typing import Any

import pytest
from PIL import Image

from aqw_char_renderer import character_svg
from aqw_char_renderer.config import RuntimeConfig
from aqw_char_renderer.stages.finalize import finalize_job
from aqw_char_renderer.stages.prepare import shared_viewbox
from aqw_char_renderer.stages.render import render_batch
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


def frame_svg(red: int, x_offset: float = 0.0) -> str:
    return f"""<?xml version="1.0" encoding="UTF-8" standalone="no"?>
<svg xmlns:ffdec="https://www.free-decompiler.com/flash" xmlns:xlink="http://www.w3.org/1999/xlink" ffdec:objectType="frame" height="20px" width="40px" xmlns="http://www.w3.org/2000/svg">
  <g transform="matrix({ZOOM}, 0.0, 0.0, {ZOOM}, 12.0, 16.0)">
    <use ffdec:characterId="11" height="10" width="20" xlink:href="#sprite0"/>
  </g>
  <defs>
    <g id="sprite0">
      <rect x="{x_offset}" y="0" width="20" height="10" fill="#{red:02x}0000"/>
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


def write_archive(root: Path, key: str, frames: list[Path]) -> Path:
    archive_path = root / f"{key}.tar.gz"
    with tarfile.open(archive_path, "w:gz") as archive:
        for index, path in enumerate(frames, start=1):
            archive.add(path, arcname=f"{index:06d}.svg")
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
        rsvg_convert=str(RSVG_CONVERT),
        cwebp=str(CWEBP),
        webpmux=str(WEBPMUX),
    )


def build_manifest(
    store: FilesystemObjectStore,
    *,
    job_id: str,
    frame_paths: list[Path],
    archive_key: str,
) -> str:
    frame_count = len(frame_paths)
    layers = character_svg.build_layers({"chest": "armor"}, weapon_type="Sword")
    viewbox = shared_viewbox(
        layers,
        {"armor": frame_paths},
        frame_count=frame_count,
        facing="right",
        zoom=ZOOM,
        max_size=512,
        padding=0,
    )
    manifest: dict[str, Any] = {
        "schema_version": 1,
        "job_id": job_id,
        "render_hash": "abc123",
        "final_key": f"renders/{job_id}.webp",
        "frame_count": frame_count,
        "frame_rate": 25.0,
        "frame_durations": [40] * frame_count,
        "viewbox": list(viewbox),
        "fields": {"strGender": "M"},
        "aliases": {"chest": "armor"},
        "weapon_type": "Sword",
        "parts": {
            "armor": {
                "root_class": "Armor",
                "archive_key": archive_key,
                "frame_count": frame_count,
                "color_rules": {},
            }
        },
        "all_color_rules": [],
        "settings": {
            "facing": "right",
            "zoom": ZOOM,
            "max_size": 512,
            "padding": 0,
            "webp_quality": 80,
            "webp_method": 4,
        },
    }
    manifest_key = f"jobs/{job_id}/prepare/manifest.json"
    store.write_json("work", manifest_key, manifest)
    return manifest_key


def test_render_then_finalize_produces_valid_animation() -> None:
    frame_count = 3
    job_id = "job-local"
    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary)
        store = FilesystemObjectStore(root / "objects")
        frame_paths = write_frames(
            root,
            [frame_svg(red=120 + index * 40, x_offset=index) for index in range(frame_count)],
        )
        archive_key = f"jobs/{job_id}/prepare/parts/armor.tar.gz"
        store.upload_file(write_archive(root, "armor", frame_paths), "work", archive_key)
        manifest_key = build_manifest(
            store,
            job_id=job_id,
            frame_paths=frame_paths,
            archive_key=archive_key,
        )
        config = pipeline_config()

        # Phase 1: probe tight per-frame bounds.
        probe = render_batch(
            job_id=job_id,
            manifest_key=manifest_key,
            batch={"index": 0, "frame_start": 1, "frame_end": frame_count},
            store=store,
            config=config,
            mode="probe",
        )
        probe_manifest = store.read_json("work", probe["batch_manifest_key"])
        assert {frame["frame"] for frame in probe_manifest["frames"]} == {1, 2, 3}
        tight_bounds = [frame["bounds"] for frame in probe_manifest["frames"]]
        # Probing a mostly-empty synthetic frame yields a small bounds box.
        for bounds in tight_bounds:
            assert bounds[2] > 0 and bounds[3] > 0

        # Phase 2: fit the global canvas.
        from aqw_char_renderer.stages.fit import fit_canvas

        fit = fit_canvas(
            job_id=job_id,
            manifest_key=manifest_key,
            probe_results=[probe],
            store=store,
            config=config,
        )

        # Phase 3: rasterize at the fitted canvas and encode.
        result = render_batch(
            job_id=job_id,
            manifest_key=manifest_key,
            batch={"index": 0, "frame_start": 1, "frame_end": frame_count},
            store=store,
            config=config,
            mode="raster",
            store_viewbox_key=fit["fitted_canvas_key"],
        )
        batch_manifest = store.read_json("work", result["batch_manifest_key"])
        assert [frame["frame"] for frame in batch_manifest["frames"]] == [1, 2, 3]
        canvases = {
            (frame["canvas_width"], frame["canvas_height"])
            for frame in batch_manifest["frames"]
        }
        assert len(canvases) == 1
        for frame in batch_manifest["frames"]:
            assert frame["x"] % 2 == 0 and frame["y"] % 2 == 0

        final = finalize_job(
            job_id=job_id,
            manifest_key=manifest_key,
            render_results=[result],
            store=store,
            config=config,
        )
        assert final["cache_hit"] is False
        assert final["frame_count"] == frame_count
        output = root / "objects" / "work" / final["final_key"]
        with Image.open(output) as animation:
            assert animation.format == "WEBP"
            assert animation.n_frames == frame_count
            assert animation.size == (final["width"], final["height"])
            assert animation.info.get("loop") == 0

        # A second finalize must hit the published cache instead of re-muxing.
        cached = finalize_job(
            job_id=job_id,
            manifest_key=manifest_key,
            render_results=[result],
            store=store,
            config=config,
        )
        assert cached["cache_hit"] is True


def test_blink_source_frames_freeze_after_one_shot() -> None:
    # The render stage freezes a one-shot blink timeline on its final frame
    # so item loops (not the eye blink) drive the animation period. This
    # locks in the 1-based archive name <-> 0-based blink index conversion.
    blink_frames = 4
    source = [
        character_svg.one_shot_source_frame_index(n - 1, one_shot_frames=blink_frames) + 1
        for n in range(1, 9)
    ]
    assert source == [1, 2, 3, 4, 4, 4, 4, 4]


def test_render_batch_rejects_out_of_range_batches() -> None:
    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary)
        store = FilesystemObjectStore(root / "objects")
        frame_paths = write_frames(root, [frame_svg(red=200)])
        archive_key = "jobs/job-local/prepare/parts/armor.tar.gz"
        store.upload_file(write_archive(root, "armor", frame_paths), "work", archive_key)
        manifest_key = build_manifest(
            store,
            job_id="job-local",
            frame_paths=frame_paths,
            archive_key=archive_key,
        )
        with pytest.raises(character_svg.CharacterSvgError, match="Invalid render batch"):
            render_batch(
                job_id="job-local",
                manifest_key=manifest_key,
                batch={"index": 1, "frame_start": 2, "frame_end": 5},
                store=store,
                config=pipeline_config(),
            )
