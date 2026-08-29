"""Validated lookup and download of the immutable source-asset corpus."""

from __future__ import annotations

import hashlib
from collections.abc import Callable, Mapping
from dataclasses import dataclass
from pathlib import Path, PurePosixPath
from typing import Any, Protocol
from urllib.parse import quote

from aqw_char_renderer.hashing import file_sha256
from aqw_char_renderer.legacy import preview_aqw_tryon as tryon


class ObjectReader(Protocol):
    def exists(self, bucket: str, key: str) -> Mapping[str, Any] | None: ...

    def download(
        self,
        bucket: str,
        key: str,
        destination: Path,
        *,
        expected_sha256: str | None = None,
    ) -> Path: ...

    def upload_file_if_absent(
        self,
        source: Path,
        bucket: str,
        key: str,
        *,
        content_type: str | None = None,
        metadata: Mapping[str, str] | None = None,
    ) -> bool: ...


@dataclass(frozen=True)
class SourceObject:
    remote_path: str
    key: str
    sha256: str
    size: int


class SourceAssetError(RuntimeError):
    pass


class SourceAssetCatalog:
    def __init__(self, payload: Any) -> None:
        if not isinstance(payload, Mapping) or payload.get("schema_version") != 1:
            raise SourceAssetError("Unsupported source asset manifest")
        raw_assets = payload.get("assets")
        if not isinstance(raw_assets, Mapping):
            raise SourceAssetError("Source asset manifest has no assets mapping")
        self.dataset_version = str(payload.get("dataset_version") or "")
        if not self.dataset_version:
            raise SourceAssetError("Source asset manifest has no dataset version")
        assets: dict[str, SourceObject] = {}
        for raw_path, raw_record in raw_assets.items():
            if not isinstance(raw_record, Mapping):
                raise SourceAssetError(f"Invalid source record for {raw_path!r}")
            normalized = tryon.normalize_asset_path(raw_path)
            record = SourceObject(
                remote_path=normalized,
                key=str(raw_record.get("key") or ""),
                sha256=str(raw_record.get("sha256") or ""),
                size=int(raw_record.get("size") or 0),
            )
            if len(record.sha256) != 64 or record.size < 3 or not record.key:
                raise SourceAssetError(f"Incomplete source record for {normalized!r}")
            folded = normalized.casefold()
            if folded in assets:
                raise SourceAssetError(f"Duplicate case-insensitive asset path: {normalized}")
            assets[folded] = record
        self.assets = assets
        self.item_database = self._named_object(payload, "item_database")
        self.character_renderer = self._named_object(payload, "character_renderer")

    @staticmethod
    def _named_object(payload: Mapping[str, Any], name: str) -> SourceObject:
        raw = payload.get(name)
        if not isinstance(raw, Mapping):
            raise SourceAssetError(f"Source manifest has no {name}")
        return SourceObject(
            remote_path=name,
            key=str(raw.get("key") or ""),
            sha256=str(raw.get("sha256") or ""),
            size=int(raw.get("size") or 0),
        )

    def get(self, remote_path: str) -> SourceObject:
        normalized = tryon.normalize_asset_path(remote_path)
        try:
            return self.assets[normalized.casefold()]
        except KeyError as error:
            raise SourceAssetError(f"Asset is absent from dataset: {normalized}") from error

    def download_asset(
        self,
        remote_path: str,
        *,
        store: ObjectReader,
        bucket: str,
        root: Path,
    ) -> Path:
        record = self.get(remote_path)
        destination = root.joinpath(*PurePosixPath(record.remote_path).parts)
        return store.download(
            bucket,
            record.key,
            destination,
            expected_sha256=record.sha256,
        )

    def resolve_and_download(
        self,
        remote_path: str,
        *,
        store: ObjectReader,
        bucket: str,
        root: Path,
        allow_official_fallback: bool,
        timeout: int,
        downloader: Callable[[str, Path], None] | None = None,
    ) -> tuple[Path, SourceObject]:
        """Resolve the manifest or immutably cache one official missing SWF."""
        normalized = tryon.normalize_asset_path(remote_path)
        try:
            record = self.get(normalized)
        except SourceAssetError:
            if not allow_official_fallback:
                raise
        else:
            destination = root.joinpath(*PurePosixPath(record.remote_path).parts)
            return (
                store.download(
                    bucket,
                    record.key,
                    destination,
                    expected_sha256=record.sha256,
                ),
                record,
            )

        path_digest = hashlib.sha256(normalized.casefold().encode("utf-8")).hexdigest()
        key = (
            f"dynamic-assets/{self.dataset_version}/"
            f"{path_digest[:2]}/{path_digest}.swf"
        )
        destination = root.joinpath(*PurePosixPath(normalized).parts)

        def cached_record(head: Mapping[str, Any]) -> SourceObject:
            metadata = head.get("Metadata")
            if not isinstance(metadata, Mapping):
                raise SourceAssetError(f"Dynamic asset has no metadata: {key}")
            sha256 = str(metadata.get("sha256") or "")
            size = int(head.get("ContentLength") or 0)
            if len(sha256) != 64 or size < 8:
                raise SourceAssetError(f"Dynamic asset metadata is invalid: {key}")
            return SourceObject(normalized, key, sha256, size)

        existing = store.exists(bucket, key)
        if existing is not None:
            record = cached_record(existing)
            return (
                store.download(
                    bucket,
                    key,
                    destination,
                    expected_sha256=record.sha256,
                ),
                record,
            )

        url = f"{tryon.GAMEFILES_URL}{quote(normalized, safe='/')}"
        if downloader is None:

            def download_official(download_url: str, output: Path) -> None:
                tryon.download_swf(download_url, output, timeout=timeout)

            downloader = download_official
        downloader(url, destination)
        if not tryon.has_valid_swf_header(destination):
            raise SourceAssetError(f"Official AQW response is not a SWF: {normalized}")
        sha256 = file_sha256(destination)
        record = SourceObject(normalized, key, sha256, destination.stat().st_size)
        created = store.upload_file_if_absent(
            destination,
            bucket,
            key,
            content_type="application/x-shockwave-flash",
            metadata={"sha256": sha256, "path-sha256": path_digest},
        )
        if created:
            return destination, record

        # Another worker won a conditional-create race. Always use the first
        # immutable object rather than silently accepting changed official data.
        existing = store.exists(bucket, key)
        if existing is None:
            raise SourceAssetError(f"Dynamic asset appeared then disappeared: {key}")
        record = cached_record(existing)
        return (
            store.download(
                bucket,
                key,
                destination,
                expected_sha256=record.sha256,
            ),
            record,
        )
