from __future__ import annotations

import tarfile
import tempfile
from pathlib import Path
from typing import Any

import pytest

from aqw_char_renderer import character_svg
from aqw_char_renderer.config import RuntimeConfig
from aqw_char_renderer.stages.finalize import _ordered_frames
from aqw_char_renderer.stages.prepare import export_frame_bounds, shared_viewbox


class MemoryStore:
    def __init__(self, values: dict[str, Any]) -> None:
        self.values = values

    def read_json(self, _bucket: str, key: str) -> Any:
        return self.values[key]

    def write_json(self, _bucket: str, key: str, value: Any) -> None:
        self.values[key] = value


def config() -> RuntimeConfig:
    return RuntimeConfig(
        source_bucket="source",
        work_bucket="work",
        job_table="jobs",
        result_queue_url="https://sqs.example/results",
        public_base_url="https://chars.example.com",
        asset_dataset_version="dev-v1",
        asset_manifest_key="datasets/dev-v1/manifest.json",
        character_renderer_key="character-renderer/dev-v1/characterB.swf",
    )


def write_export(path: Path, body: str) -> Path:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(body, encoding="utf-8")
    return path


FFDEC_EXPORT = """<?xml version="1.0" encoding="UTF-8" standalone="no"?>
<svg xmlns:ffdec="https://www.free-decompiler.com/flash" xmlns:xlink="http://www.w3.org/1999/xlink" ffdec:objectType="frame" height="10px" width="20px" xmlns="http://www.w3.org/2000/svg">
  <g transform="matrix(2.0, 0.0, 0.0, 2.0, 6.0, 8.0)">
    <use ffdec:characterId="11" height="5" transform="matrix(1.0, 0.0, 0.0, 1.0, 0.0, 0.95)" width="10" xlink:href="#sprite0"/>
  </g>
  <defs>
    <g id="sprite0">
      <rect width="10" height="5" transform="matrix(9, 0, 0, 9, 0, 0)" fill="#ff0000"/>
    </g>
  </defs>
</svg>
"""

FFDEC_EXPORT_DEFS_FIRST = """<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" width="20px" height="10px">
  <defs>
    <g id="shape0"><rect width="10" height="5" transform="matrix(9,0,0,9,0,0)"/></g>
  </defs>
  <g transform="matrix(2,0,0,2,6,8)">
    <use xlink:href="#shape0"/>
  </g>
</svg>
"""


def test_export_frame_bounds_matches_full_import() -> None:
    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary)
        for name, body in (
            ("defs_last.svg", FFDEC_EXPORT),
            ("defs_first.svg", FFDEC_EXPORT_DEFS_FIRST),
        ):
            source = write_export(root / name, body)
            assert export_frame_bounds(source, zoom=2) == (-3.0, -4.0, 10.0, 5.0)
            imported = character_svg.import_ffdec_symbol(
                "test", source, zoom=2, color_rules={}, root_class="Test"
            )
            assert export_frame_bounds(source, zoom=2) == imported.bounds


def test_export_frame_bounds_treats_zero_area_exports_as_empty() -> None:
    with tempfile.TemporaryDirectory() as temporary:
        source = write_export(
            Path(temporary) / "empty.svg",
            '<svg xmlns="http://www.w3.org/2000/svg" width="0px" height="0px"/>',
        )
        assert export_frame_bounds(source, zoom=2) is None


def test_export_frame_bounds_rejects_wrong_zoom() -> None:
    with tempfile.TemporaryDirectory() as temporary:
        source = write_export(Path(temporary) / "frame.svg", FFDEC_EXPORT)
        with pytest.raises(character_svg.CharacterSvgError, match="crop/zoom matrix"):
            export_frame_bounds(source, zoom=4)


def test_shared_viewbox_unions_all_frames_with_margin() -> None:
    layers = [
        character_svg.Layer("chest", "armor", (1.0, 0.0, 0.0, 1.0, 0.0, 0.0)),
        character_svg.Layer("weapon", "weapon", (1.0, 0.0, 0.0, 1.0, 10.0, 0.0)),
    ]
    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary)
        exports = {
            "armor": [
                write_export(root / "armor" / f"{index:06d}.svg", FFDEC_EXPORT)
                for index in (1, 2)
            ],
            "weapon": [
                write_export(root / "weapon" / "000001.svg", FFDEC_EXPORT_DEFS_FIRST),
            ],
        }
        scale = character_svg.CHARACTER_DISPLAY_SCALE
        viewbox = shared_viewbox(
            layers,
            exports,
            frame_count=2,
            facing="right",
            zoom=2,
            max_size=1024,
            padding=0,
        )
        # Layer bounds before the display scale and margin:
        #   armor frames: (-3, -4, 10, 5); weapon frame: (7, -4, 10, 5)
        tight = (-3.0 * scale, -4.0 * scale, 20.0 * scale, 5.0 * scale)
        margin = max(tight[2], tight[3]) * 0.1 + 2
        assert viewbox == pytest.approx(
            (tight[0] - margin, tight[1] - margin, tight[2] + 2 * margin, tight[3] + 2 * margin)
        )


def test_shared_viewbox_requires_visible_layers() -> None:
    layers = [character_svg.Layer("chest", "armor", (1.0, 0.0, 0.0, 1.0, 0.0, 0.0))]
    with tempfile.TemporaryDirectory() as temporary:
        exports = {
            "armor": [
                write_export(
                    Path(temporary) / "000001.svg",
                    '<svg xmlns="http://www.w3.org/2000/svg" width="0px" height="0px"/>',
                ),
            ],
        }
        with pytest.raises(character_svg.CharacterSvgError, match="no visible layers"):
            shared_viewbox(
                layers,
                exports,
                frame_count=1,
                facing="right",
                zoom=2,
                max_size=1024,
                padding=0,
            )


def test_part_archive_round_trips_frame_names() -> None:
    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary)
        archive_path = root / "part.tar.gz"
        with tarfile.open(archive_path, "w:gz") as archive:
            for index in (1, 2, 3):
                frame = write_export(root / "src" / f"{index:06d}.svg", FFDEC_EXPORT)
                archive.add(frame, arcname=f"{index:06d}.svg")
        target = root / "extracted"
        target.mkdir()
        with tarfile.open(archive_path) as archive:
            archive.extractall(target, filter="data")
        assert sorted(path.name for path in target.iterdir()) == [
            "000001.svg",
            "000002.svg",
            "000003.svg",
        ]


def test_finalizer_orders_batches_and_rejects_duplicates() -> None:
    store = MemoryStore(
        {
            "batch-1": {
                "job_id": "job",
                "frames": [{"frame": 2, "canvas_width": 100, "canvas_height": 80}],
            },
            "batch-0": {
                "job_id": "job",
                "frames": [{"frame": 1, "canvas_width": 100, "canvas_height": 80}],
            },
        }
    )
    ordered = _ordered_frames(
        "job",
        2,
        [{"batch_manifest_key": "batch-1"}, {"batch_manifest_key": "batch-0"}],
        store=store,
        config=config(),
    )
    assert [frame["frame"] for frame in ordered] == [1, 2]
    with pytest.raises(Exception, match="Duplicate"):
        _ordered_frames(
            "job",
            2,
            [{"batch_manifest_key": "batch-0"}, {"batch_manifest_key": "batch-0"}],
            store=store,
            config=config(),
        )
