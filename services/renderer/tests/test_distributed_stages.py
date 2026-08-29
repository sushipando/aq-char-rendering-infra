from __future__ import annotations

from typing import Any

import pytest

from aqw_char_renderer.config import RuntimeConfig
from aqw_char_renderer.stages.bounds import reduce_bounds
from aqw_char_renderer.stages.finalize import _ordered_frames


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


def test_bounds_reducer_requires_exactly_one_record_per_frame() -> None:
    store = MemoryStore(
        {
            "prepare": {
                "job_id": "job",
                "frame_count": 2,
                "settings": {"max_size": 100, "padding": 0},
            },
            "batch": {
                "job_id": "job",
                "frames": [
                    {"frame": 1, "bounds": [-10, -20, 50, 100]},
                    {"frame": 2, "bounds": [0, -30, 90, 80]},
                ],
            },
        }
    )
    result = reduce_bounds(
        job_id="job",
        manifest_key="prepare",
        compose_results=[{"batch_manifest_key": "batch"}],
        store=store,
        config=config(),
    )
    assert result["shared_canvas_key"] == "jobs/job/shared-canvas.json"
    assert store.values[result["shared_canvas_key"]]["viewbox"] == [-10.0, -30.0, 100.0, 110.0]


def test_bounds_reducer_rejects_missing_frame() -> None:
    store = MemoryStore(
        {
            "prepare": {
                "job_id": "job",
                "frame_count": 2,
                "settings": {"max_size": 100, "padding": 0},
            },
            "batch": {"job_id": "job", "frames": [{"frame": 1, "bounds": [0, 0, 10, 10]}]},
        }
    )
    with pytest.raises(ValueError, match="incomplete"):
        reduce_bounds(
            job_id="job",
            manifest_key="prepare",
            compose_results=[{"batch_manifest_key": "batch"}],
            store=store,
            config=config(),
        )


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
