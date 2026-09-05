from __future__ import annotations

import tempfile
from hashlib import sha256
from pathlib import Path

import pytest

from aqw_char_renderer.batching import partition_frames
from aqw_char_renderer.config import RuntimeConfig
from aqw_char_renderer.contracts import CacheSettings, ContractError, JobRequest
from aqw_char_renderer.geometry import shared_canvas, union_bounds
from aqw_char_renderer.handlers.launcher import hydrate_request_defaults
from aqw_char_renderer.hashing import canonical_sha256, render_key
from aqw_char_renderer.source_assets import SourceAssetCatalog, SourceAssetError
from aqw_char_renderer.storage import FilesystemObjectStore, StorageError, validate_key


class DynamicAssetStore:
    def __init__(self) -> None:
        self.objects: dict[str, bytes] = {}
        self.metadata: dict[str, dict[str, str]] = {}

    def exists(self, _bucket: str, key: str) -> dict | None:
        payload = self.objects.get(key)
        if payload is None:
            return None
        return {
            "ContentLength": len(payload),
            "Metadata": self.metadata[key],
        }

    def download(
        self,
        _bucket: str,
        key: str,
        destination: Path,
        *,
        expected_sha256: str | None = None,
    ) -> Path:
        payload = self.objects[key]
        if expected_sha256 is not None:
            assert sha256(payload).hexdigest() == expected_sha256
        destination.parent.mkdir(parents=True, exist_ok=True)
        destination.write_bytes(payload)
        return destination

    def upload_file_if_absent(
        self,
        source: Path,
        _bucket: str,
        key: str,
        *,
        content_type: str | None = None,
        metadata: dict[str, str] | None = None,
    ) -> bool:
        assert content_type == "application/x-shockwave-flash"
        if key in self.objects:
            return False
        self.objects[key] = source.read_bytes()
        self.metadata[key] = dict(metadata or {})
        return True


def request_payload() -> dict:
    return {
        "schema_version": 1,
        "job_id": "8d1c70fd-6c7a-4abc-a539-014575b09078",
        "created_at": "2026-08-28T20:00:00Z",
        "discord": {
            "user_id": "123456789012345678",
            "guild_id": "234567890123456789",
            "channel_id": "345678901234567890",
        },
        "render": {
            "username": "  Sora   to Hoshi ",
            "base_items": False,
            "show_hidden": False,
            "facing": "right",
            "override": None,
            "complete_loop": True,
            "max_frames": 360,
            "subframe_start": 1,
            "zoom": 2,
            "raster_size": 2048,
            "output_size": 1024,
            "padding": 0,
            "webp_quality": 85,
            "webp_method": 4,
            "raster_backend": "resvg",
        },
    }


def test_request_contract_normalizes_username_and_round_trips() -> None:
    request = JobRequest.from_dict(request_payload())
    assert request.render.username == "Sora to Hoshi"
    assert request.bounds_mode == "inline"
    assert request.component_raster_mode == "inline"
    assert request.to_dict()["bounds_mode"] == "inline"
    assert request.to_dict()["component_raster_mode"] == "inline"
    assert JobRequest.from_dict(request.to_dict()) == request


def test_request_contract_validates_bounds_mode() -> None:
    payload = request_payload()
    payload["bounds_mode"] = "distributed"
    assert JobRequest.from_dict(payload).bounds_mode == "distributed"

    payload["bounds_mode"] = "automatic"
    with pytest.raises(ContractError, match="bounds_mode"):
        JobRequest.from_dict(payload)


def test_request_contract_validates_component_raster_mode() -> None:
    payload = request_payload()
    payload["component_raster_mode"] = "distributed"
    assert JobRequest.from_dict(payload).component_raster_mode == "distributed"

    payload["component_raster_mode"] = "automatic"
    with pytest.raises(ContractError, match="component_raster_mode"):
        JobRequest.from_dict(payload)


def test_request_contract_validates_optional_cache_controls() -> None:
    payload = request_payload()
    payload["cache"] = {"render": False, "bounds": False}
    request = JobRequest.from_dict(payload)
    assert request.cache == CacheSettings(
        render=False,
        animation=True,
        vectors=True,
        bounds=False,
        components=True,
    )
    assert request.to_dict()["cache"] == {
        "render": False,
        "animation": True,
        "vectors": True,
        "bounds": False,
        "components": True,
    }

    payload["cache"] = {"vectors": "false"}
    with pytest.raises(ContractError, match="cache.vectors"):
        JobRequest.from_dict(payload)


def test_request_contract_accepts_bounded_matching_appearance() -> None:
    payload = request_payload()
    payload["appearance"] = {
        "strName": "Sora to Hoshi",
        "strGender": "F",
        "strClassFile": "Example.swf",
    }

    request = JobRequest.from_dict(payload)

    assert request.appearance == {
        "strClassFile": "Example.swf",
        "strGender": "F",
        "strName": "Sora to Hoshi",
    }


@pytest.mark.parametrize(
    "appearance",
    [
        {"strName": "Different User"},
        {"strName": "Sora to Hoshi", "bad-key": "value"},
        {"strName": "Sora to Hoshi", "strClassFile": "x" * 2_049},
    ],
)
def test_request_contract_rejects_untrusted_appearance(appearance: dict[str, str]) -> None:
    payload = request_payload()
    payload["appearance"] = appearance

    with pytest.raises(ContractError):
        JobRequest.from_dict(payload)


def test_launcher_hydrates_sparse_request_from_runtime_tuning() -> None:
    payload = request_payload()
    payload["render"] = {"username": "Artix"}
    config = RuntimeConfig(
        source_bucket="source",
        work_bucket="work",
        job_table="jobs",
        result_queue_url="https://sqs.example/results",
        public_base_url="https://chars.example.com",
        asset_dataset_version="dev-v1",
        asset_manifest_key="datasets/dev-v1/manifest.json",
        character_renderer_key="character-renderer/dev-v1/characterB.swf",
        default_raster_size=2048,
        default_output_size=1024,
        default_zoom=1.5,
        default_max_frames=120,
        default_webp_quality=80,
    )

    request = JobRequest.from_dict(hydrate_request_defaults(payload, config))

    assert request.render.raster_size == 2048
    assert request.render.output_size == 1024
    assert request.render.zoom == 1.5
    assert request.render.max_frames == 120
    assert request.render.webp_quality == 80


@pytest.mark.parametrize(
    ("path", "value"),
    [
        (("schema_version",), 2),
        (("render", "username"), "https://example.com/bad.swf"),
        (("render", "raster_size"), 4097),
        (("render", "output_size"), 2049),
        (("render", "padding"), 1024),
        (("discord", "user_id"), "not-a-user"),
    ],
)
def test_request_contract_rejects_unsafe_values(path: tuple[str, ...], value: object) -> None:
    payload = request_payload()
    target = payload
    for component in path[:-1]:
        target = target[component]
    target[path[-1]] = value
    with pytest.raises(ContractError):
        JobRequest.from_dict(payload)


def test_request_contract_rejects_unknown_fields() -> None:
    payload = request_payload()
    payload["render"]["arbitrary_url"] = "https://example.com/item.swf"
    with pytest.raises(ContractError, match="unsupported field"):
        JobRequest.from_dict(payload)


def test_request_contract_accepts_raster_backend_choices() -> None:
    payload = request_payload()
    payload["render"]["raster_backend"] = "thorvg"
    assert JobRequest.from_dict(payload).render.raster_backend == "thorvg"

    payload2 = request_payload()
    payload2["render"]["raster_backend"] = "not-a-backend"
    with pytest.raises(ContractError, match="raster_backend"):
        JobRequest.from_dict(payload2)


def test_request_contract_rejects_output_larger_than_raster() -> None:
    payload = request_payload()
    payload["render"]["raster_size"] = 1024
    payload["render"]["output_size"] = 2048

    with pytest.raises(ContractError, match="must not exceed"):
        JobRequest.from_dict(payload)


def test_launcher_normalizes_legacy_max_size() -> None:
    payload = request_payload()
    payload["render"] = {"username": "Artix", "max_size": 1024}
    config = RuntimeConfig(
        source_bucket="source",
        work_bucket="work",
        job_table="jobs",
        result_queue_url="https://sqs.example/results",
        public_base_url="https://chars.example.com",
        asset_dataset_version="dev-v1",
        asset_manifest_key="datasets/dev-v1/manifest.json",
        character_renderer_key="character-renderer/dev-v1/characterB.swf",
    )

    request = JobRequest.from_dict(hydrate_request_defaults(payload, config))

    assert request.render.raster_size == 1024
    assert request.render.output_size == 1024


def test_frame_batches_are_contiguous_and_deterministic() -> None:
    batches = partition_frames(360, 30)
    assert len(batches) == 12
    assert batches[0].to_dict() == {"index": 0, "frame_start": 1, "frame_end": 30}
    assert batches[-1].to_dict() == {"index": 11, "frame_start": 331, "frame_end": 360}
    assert [
        frame for batch in batches for frame in range(batch.frame_start, batch.frame_end + 1)
    ] == list(range(1, 361))


def test_shared_canvas_unions_negative_bounds_and_applies_pixel_padding() -> None:
    bounds = [(-10, -20, 50, 100), (20, -30, 80, 50)]
    assert union_bounds(bounds) == (-10, -30, 110, 110)
    assert shared_canvas(bounds, max_size=120, padding=5) == (-15, -35, 120, 120)


def test_canonical_hash_is_order_independent_and_setting_sensitive() -> None:
    assert canonical_sha256({"b": 2, "a": 1}) == canonical_sha256({"a": 1, "b": 2})
    assert canonical_sha256({"quality": 85}) != canonical_sha256({"quality": 80})
    digest = "a" * 64
    assert render_key("v3", 85, 2048, digest) == f"renders/v3/q85/2048/aa/{digest}.webp"


def test_filesystem_store_round_trips_json_and_blocks_traversal() -> None:
    with tempfile.TemporaryDirectory() as temporary:
        store = FilesystemObjectStore(Path(temporary))
        store.write_json("work", "jobs/id/manifest.json", {"ok": True})
        assert store.read_json("work", "jobs/id/manifest.json") == {"ok": True}
        with pytest.raises(StorageError):
            store.write_json("work", "../outside.json", {})
        with pytest.raises(StorageError):
            validate_key("/absolute")


def test_source_catalog_is_case_insensitive_but_rejects_collisions() -> None:
    payload = {
        "schema_version": 1,
        "dataset_version": "dev-v1",
        "item_database": {"key": "datasets/dev-v1/item_db.json", "sha256": "a" * 64, "size": 3},
        "character_renderer": {
            "key": "character-renderer/dev-v1/characterB.swf",
            "sha256": "b" * 64,
            "size": 3,
        },
        "assets": {
            "items/swords/Test.swf": {
                "key": "datasets/dev-v1/swf/items/swords/Test.swf",
                "sha256": "c" * 64,
                "size": 3,
            }
        },
    }
    catalog = SourceAssetCatalog(payload)
    assert catalog.get("ITEMS/SWORDS/test.swf").remote_path == "items/swords/Test.swf"
    payload["assets"]["items/swords/test.swf"] = payload["assets"]["items/swords/Test.swf"]
    with pytest.raises(SourceAssetError, match="Duplicate"):
        SourceAssetCatalog(payload)


def test_missing_manifest_asset_is_fetched_only_from_official_host_then_cached() -> None:
    payload = {
        "schema_version": 1,
        "dataset_version": "dev-v1",
        "item_database": {
            "key": "datasets/dev-v1/item_db.json",
            "sha256": "a" * 64,
            "size": 8,
        },
        "character_renderer": {
            "key": "character-renderer/dev-v1/characterB.swf",
            "sha256": "b" * 64,
            "size": 8,
        },
        "assets": {},
    }
    catalog = SourceAssetCatalog(payload)
    store = DynamicAssetStore()
    requested_urls: list[str] = []

    def downloader(url: str, destination: Path) -> None:
        requested_urls.append(url)
        destination.parent.mkdir(parents=True, exist_ok=True)
        destination.write_bytes(b"FWS12345")

    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary)
        path, record = catalog.resolve_and_download(
            "classes/F/Yami Armor.swf",
            store=store,
            bucket="source",
            root=root / "first",
            allow_official_fallback=True,
            timeout=15,
            downloader=downloader,
        )
        cached_path, cached_record = catalog.resolve_and_download(
            "classes/F/Yami Armor.swf",
            store=store,
            bucket="source",
            root=root / "second",
            allow_official_fallback=True,
            timeout=15,
            downloader=lambda *_args: pytest.fail("cached SWF should not be downloaded"),
        )
        assert path.read_bytes() == b"FWS12345"
        assert cached_path.read_bytes() == b"FWS12345"

    assert requested_urls == ["https://game.aq.com/game/gamefiles/classes/F/Yami%20Armor.swf"]
    assert cached_record == record
    assert record.key.startswith("dynamic-assets/dev-v1/")
