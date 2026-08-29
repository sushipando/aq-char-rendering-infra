"""Prepare phases: resolve appearance, export FFDec parts per source, finish manifest.

Split across three Lambda phases so the expensive per-source FFDec export runs
in parallel (one Lambda per source SWF) instead of serially inside a single
prepare invocation:

- prepare_resolve: resolve appearance, build symbol requests, hash, cache
  check, and publish prepare-input.json.
- prepare_export_source: one source SWF, export its symbol timelines to
  per-symbol tar.gz archives, upload color rules + authored placement colors.
- prepare_finish: reconstruct raw exports from the archives, detect loop,
  compute the shared viewbox, and write the render manifest.
"""

from __future__ import annotations

import json
import math
import re
import tarfile
import tempfile
import time
from collections.abc import Mapping, Sequence
from dataclasses import asdict
from pathlib import Path
from typing import Any, Protocol

from botocore.exceptions import ClientError

from aqw_char_renderer import character_svg
from aqw_char_renderer.batching import partition_frames
from aqw_char_renderer.config import RuntimeConfig
from aqw_char_renderer.contracts import JobRequest
from aqw_char_renderer.geometry import shared_canvas, union_bounds
from aqw_char_renderer.hashing import canonical_sha256, render_key
from aqw_char_renderer.legacy import preview_aqw_tryon as tryon
from aqw_char_renderer.legacy import render_swf_items as item_renderer
from aqw_char_renderer.source_assets import SourceAssetCatalog, SourceObject
from aqw_char_renderer.storage import StorageError
from aqw_char_renderer.structured_logging import log_event


class StageStore(Protocol):
    def exists(self, bucket: str, key: str) -> Mapping[str, Any] | None: ...
    def download(
        self,
        bucket: str,
        key: str,
        destination: Path,
        *,
        expected_sha256: str | None = None,
    ) -> Path: ...
    def upload_file(self, source: Path, bucket: str, key: str, **kwargs: Any) -> None: ...
    def upload_file_if_absent(
        self, source: Path, bucket: str, key: str, **kwargs: Any
    ) -> bool: ...
    def read_json(self, bucket: str, key: str) -> Any: ...
    def write_json(self, bucket: str, key: str, value: Any) -> None: ...


def _override(
    request: JobRequest,
    fields: dict[str, str],
    catalog: SourceAssetCatalog,
    store: StageStore,
    config: RuntimeConfig,
    asset_root: Path,
    database: Path,
) -> tuple[dict[str, Path], dict[Path, SourceObject]]:
    selected = request.render.override
    if selected is None:
        return {}, {}
    record = tryon.load_item(database, selected.item_id)
    database_slot = str(record.get("slot") or "")
    slot = tryon.effective_slot(database_slot)
    if slot not in character_svg.SUPPORTED_OVERRIDE_SLOTS:
        raise character_svg.CharacterSvgError(
            f"Item {selected.item_id} has unsupported on-character slot {database_slot!r}"
        )
    if selected.slot is not None and selected.slot != slot:
        raise character_svg.CharacterSvgError(
            f"Override slot {selected.slot!r} conflicts with item slot {slot!r}"
        )
    raw_file = str(record.get("file") or "")
    if not raw_file:
        raise character_svg.CharacterSvgError(f"Item {selected.item_id} has no SWF path")
    remote_path = (
        f"classes/{fields.get('strGender', 'M').upper()}/{raw_file}"
        if slot == "armor" and "/" not in raw_file
        else tryon.normalize_asset_path(raw_file)
    )
    source, source_record = catalog.resolve_and_download(
        remote_path,
        store=store,
        bucket=config.source_bucket,
        root=asset_root,
        allow_official_fallback=config.allow_official_asset_fallback,
        timeout=config.official_asset_timeout_seconds,
    )
    link = tryon.infer_export_link(source, slot=slot, gender=fields.get("strGender", "M"))
    weapon_type = tryon.infer_weapon_type(source, database_slot) if slot == "weapon" else None
    synthetic = character_svg._apply_override(
        fields,
        slot=slot,
        source=source,
        link=link,
        name=str(record.get("name") or f"Item {selected.item_id}"),
        weapon_type=weapon_type,
        custom=not request.render.base_items,
    )
    return {synthetic.casefold(): source}, {source: source_record}


def _source_color_rules(
    source: Path,
    record: SourceObject,
    *,
    store: StageStore,
    config: RuntimeConfig,
    scripts_root: Path,
) -> dict[str, tuple[str, str]]:
    """Color rules for one source SWF, cached by content hash.

    Rules are a pure function of the SWF bytes, so computing them once per
    source SHA-256 and reusing across jobs removes an FFDec ActionScript
    export (~2s each) from every repeat render.
    """
    cache_key = f"color-rules/{record.sha256}.json"
    cached: dict[str, tuple[str, str]] | None = None
    try:
        payload = store.read_json(config.work_bucket, cache_key)
        cached = {name: tuple(rule) for name, rule in payload.items()}
    except ClientError as error:
        # NoSuchKey is a normal cache miss; anything else means we cannot
        # trust the cache and must recompute.
        if error.response.get("Error", {}).get("Code") not in {"NoSuchKey", "NoSuchBucket", "404"}:
            log_event(
                "color_rules_cache_read_failed",
                key=cache_key,
                error=str(error),
            )
    except (StorageError, KeyError, json.JSONDecodeError, OSError) as error:
        log_event("color_rules_cache_corrupt", key=cache_key, error=str(error))
    if cached is not None:
        return cached
    computed = character_svg.parse_color_scripts(
        source,
        ffdec=config.ffdec_path,
        destination=scripts_root,
    )
    try:
        store.write_json(
            config.work_bucket,
            cache_key,
            {k: list(v) for k, v in computed.items()},
        )
    except (StorageError, ClientError, OSError) as error:
        # A failed cache write must never fail the render.
        log_event("color_rules_cache_write_failed", key=cache_key, error=str(error))
    return computed


_SVG_ROOT_TAG_RE = re.compile(rb"<svg\b[^>]*>", re.DOTALL)
_MATRIX_ATTR_RE = re.compile(rb'transform="(matrix\([^)]*\))"')


def _svg_dimension(root_tag: bytes, name: str) -> float | None:
    match = re.search(rb'\b' + name.encode("ascii") + rb'="([^"]*)"', root_tag)
    if match is None:
        return None
    value = match.group(1).decode("utf-8", "replace").strip()
    numeric = re.fullmatch(r"([-+0-9.eE]+)(?:px)?", value)
    if numeric is None:
        return None
    try:
        parsed = float(numeric.group(1))
    except ValueError:
        return None
    return parsed if math.isfinite(parsed) and parsed >= 0 else None


def export_frame_bounds(path: Path, zoom: float) -> tuple[float, float, float, float] | None:
    """Bounds of one raw FFDec frame export, parsed from its header only.

    Mirrors import_ffdec_symbol's bounds math without paying for a full XML
    parse of every frame. Returns None for intentionally empty exports.
    """
    data = path.read_bytes()
    root_match = _SVG_ROOT_TAG_RE.search(data)
    if root_match is None:
        raise character_svg.CharacterSvgError(f"FFDec SVG has no root element: {path}")
    width = _svg_dimension(root_match.group(0), "width")
    height = _svg_dimension(root_match.group(0), "height")
    if width is None or height is None:
        raise character_svg.CharacterSvgError(f"FFDec SVG has no usable dimensions: {path}")
    if width == 0 or height == 0:
        return None
    body = data
    defs_start = data.find(b"<defs")
    defs_end = data.find(b"</defs>")
    if 0 <= defs_start < defs_end:
        body = data[:defs_start] + data[defs_end + len(b"</defs>"):]
    transform_match = _MATRIX_ATTR_RE.search(body)
    matrix = character_svg.parse_matrix(
        transform_match.group(1).decode("utf-8", "replace") if transform_match else None
    )
    if matrix is None:
        raise character_svg.CharacterSvgError(f"FFDec frame wrapper has no matrix: {path}")
    a, b, c, d, e, f = matrix
    if abs(b) > 1e-8 or abs(c) > 1e-8 or abs(a - zoom) > 1e-5 or abs(d - zoom) > 1e-5:
        raise character_svg.CharacterSvgError(
            f"Unexpected FFDec crop/zoom matrix {matrix} in {path}"
        )
    return (-e / zoom, -f / zoom, width / zoom, height / zoom)


def shared_viewbox(
    layers: Sequence[character_svg.Layer],
    raw_exports: Mapping[str, Sequence[Path]],
    *,
    frame_count: int,
    facing: str,
    zoom: float,
    max_size: int,
    padding: int,
) -> tuple[float, float, float, float]:
    """Union every composed frame's vector bounds into one stable animation canvas.

    This replaces the old compose->bounds reduce: rasterizing every frame
    against this canvas keeps one pixel scale for the whole animation while
    webpmux still receives delta-cropped frames with offsets.
    """
    direction = 1.0 if facing == "right" else -1.0
    outer = (
        direction * character_svg.CHARACTER_DISPLAY_SCALE,
        0.0,
        0.0,
        character_svg.CHARACTER_DISPLAY_SCALE,
        0.0,
        0.0,
    )
    transformed: list[tuple[float, float, float, float]] = []
    for layer in layers:
        matrix = item_renderer.compose_transforms(outer, layer.transform)
        for frame in raw_exports[layer.symbol_key][:frame_count]:
            bounds = export_frame_bounds(frame, zoom)
            if bounds is None:
                continue
            transformed.append(character_svg._transformed_bounds(bounds, matrix))
    try:
        tight = union_bounds(transformed)
    except ValueError as error:
        raise character_svg.CharacterSvgError(
            "Character composition produced no visible layers"
        ) from error
    # Match compose_svg's conservative page margin so filter glow is not clipped.
    margin = max(tight[2], tight[3]) * 0.1 + 2
    return shared_canvas(
        [(tight[0] - margin, tight[1] - margin, tight[2] + margin * 2, tight[3] + margin * 2)],
        max_size=max_size,
        padding=padding,
    )


def _extract_archive(archive_path: Path, target: Path) -> None:
    target.mkdir(parents=True, exist_ok=True)
    with tarfile.open(archive_path) as archive:
        archive.extractall(target, filter="data")


def prepare_resolve(
    request: JobRequest,
    *,
    store: StageStore,
    config: RuntimeConfig,
    flashvars: Mapping[str, str] | None = None,
) -> dict[str, Any]:
    """Phase 1: resolve appearance, build symbol requests, cache check.

    Publishes prepare-input.json with everything the export and finish phases
    need, then returns the source list to fan out the FFDec export Map. On a
    cache hit returns the cached result immediately.
    """
    started = time.perf_counter()
    timings: dict[str, float] = {}

    def mark(name: str, phase: float) -> None:
        timings[name] = timings.get(name, 0.0) + (time.perf_counter() - phase) * 1000

    phase = time.perf_counter()
    manifest_payload = store.read_json(config.source_bucket, config.asset_manifest_key)
    catalog = SourceAssetCatalog(manifest_payload)
    mark("manifest_read_ms", phase)
    if catalog.dataset_version != config.asset_dataset_version:
        raise character_svg.CharacterSvgError(
            "Configured asset dataset does not match its source manifest"
        )

    with tempfile.TemporaryDirectory(prefix=f"aqw-resolve-{request.job_id}-") as temporary:
        root = Path(temporary)
        phase = time.perf_counter()
        fields = (
            {str(key): str(value) for key, value in flashvars.items()}
            if flashvars is not None
            else tryon.fetch_character_flashvars(request.render.username, timeout=15)
        )
        if request.render.show_hidden:
            fields["ia1"] = str(character_svg._visibility_flags(fields) & ~0b111)
        mark("flashvars_ms", phase)

        asset_root = root / "assets"
        phase = time.perf_counter()
        database = store.download(
            config.source_bucket,
            catalog.item_database.key,
            root / "item_db.json",
            expected_sha256=catalog.item_database.sha256,
        )
        mark("item_db_download_ms", phase)
        explicit, source_records = _override(
            request, fields, catalog, store, config, asset_root, database
        )
        assets = character_svg.appearance_assets(
            fields, use_cosmetics=not request.render.base_items
        )
        sources: dict[str, Path] = {}
        phase = time.perf_counter()
        for slot, asset in assets.items():
            source = explicit.get(asset.remote_path.casefold())
            if source is None:
                source, record = catalog.resolve_and_download(
                    asset.remote_path,
                    store=store,
                    bucket=config.source_bucket,
                    root=asset_root,
                    allow_official_fallback=config.allow_official_asset_fallback,
                    timeout=config.official_asset_timeout_seconds,
                )
                source_records[source] = record
            sources[slot] = source
        mark("asset_download_ms", phase)

        phase = time.perf_counter()
        character_renderer = store.download(
            config.source_bucket,
            catalog.character_renderer.key,
            root / "characterB.swf",
            expected_sha256=catalog.character_renderer.sha256,
        )
        if not tryon.has_valid_swf_header(character_renderer):
            raise character_svg.CharacterSvgError("Source characterB.swf is invalid")
        gender = fields.get("strGender", "M").upper()
        requests, aliases, warnings = character_svg.build_symbol_requests(
            assets,
            sources,
            character_renderer=character_renderer,
            gender=gender,
        )
        mark("renderer_download_and_symbol_ms", phase)
        weapon_type = assets.get(
            "weapon", character_svg.AppearanceAsset("", "", "", "Sword")
        ).weapon_type
        source_records.setdefault(character_renderer, catalog.character_renderer)

        appearance_for_hash = {
            "gender": gender,
            "visibility": fields.get("ia1", "0"),
            "colors": {
                key: value for key, value in sorted(fields.items()) if key.startswith("intColor")
            },
            "assets": {slot: asdict(asset) for slot, asset in sorted(assets.items())},
            "symbols": [
                {
                    "key": symbol.key,
                    "class_name": symbol.class_name,
                    "character_id": symbol.character_id,
                    "frame": symbol.frame,
                    "source_sha256": source_records[symbol.source].sha256,
                }
                for symbol in requests
            ],
            "override": (
                asdict(request.render.override) if request.render.override is not None else None
            ),
        }
        digest = canonical_sha256(
            {
                "schema_version": 1,
                "renderer_version": config.renderer_version,
                "character_renderer_sha256": catalog.character_renderer.sha256,
                "ffdec_version": "26.2.1",
                "libwebp_version": "1.5.0",
                "asset_dataset_version": config.asset_dataset_version,
                "appearance": appearance_for_hash,
                "settings": request.render.to_dict(),
            }
        )
        final_key = render_key(
            config.renderer_version,
            request.render.webp_quality,
            request.render.max_size,
            digest,
        )
        cached = (
            store.exists(config.work_bucket, final_key)
            if config.render_cache_enabled
            else None
        )
        if cached is not None:
            metadata = dict(cached.get("Metadata") or {})
            log_event(
                "prepare_resolve_profile",
                job_id=request.job_id,
                cache_hit=True,
                total_ms=round((time.perf_counter() - started) * 1000, 1),
                **{key: round(value, 1) for key, value in sorted(timings.items())},
            )
            return {
                "schema_version": 1,
                "job_id": request.job_id,
                "cache_hit": True,
                "render_hash": digest,
                "final_key": final_key,
                "result": {
                    "url": f"{config.public_base_url}/{final_key}",
                    "frame_count": int(metadata.get("frame-count", 0)),
                    "width": int(metadata.get("width", 0)),
                    "height": int(metadata.get("height", 0)),
                    "duration_ms": int(metadata.get("duration-ms", 0)),
                    "bytes": int(cached.get("ContentLength", 0)),
                    "cache_hit": True,
                },
            }

        export_frame_count = (
            request.render.max_frames
            + min(character_svg.LOOP_VALIDATION_FRAMES, request.render.max_frames)
            if request.render.complete_loop
            else 1
        )
        # Group the requests by their source so each export Lambda handles one
        # SWF and its full timeline (FFDec -sublength is prefix-only, so a
        # single source cannot be split across frame ranges).
        requests_by_source: dict[Path, list[character_svg.SymbolRequest]] = {}
        for symbol in requests:
            requests_by_source.setdefault(symbol.source, []).append(symbol)
        input_key = f"jobs/{request.job_id}/prepare/input.json"
        store.write_json(
            config.work_bucket,
            input_key,
            {
                "schema_version": 1,
                "job_id": request.job_id,
                "render_hash": digest,
                "final_key": final_key,
                "export_frame_count": export_frame_count,
                "fields": dict(sorted(fields.items())),
                "aliases": dict(sorted(aliases.items())),
                "weapon_type": weapon_type,
                "settings": request.render.to_dict(),
                "warnings": warnings,
                "character_renderer": {
                    "key": catalog.character_renderer.key,
                    "sha256": catalog.character_renderer.sha256,
                },
                "sources": [
                    {
                        "idx": index,
                        "key": source_records[source].key,
                        "sha256": source_records[source].sha256,
                        "remote_path": source_records[source].remote_path,
                        "requests": [
                            {
                                "key": symbol.key,
                                "class_name": symbol.class_name,
                                "character_id": symbol.character_id,
                                "frame": symbol.frame,
                            }
                            for symbol in sorted(
                                group,
                                key=lambda item: item.key,
                            )
                        ],
                    }
                    for index, (source, group) in enumerate(
                        sorted(requests_by_source.items(), key=lambda item: str(item[0]))
                    )
                ],
            },
        )
        log_event(
            "prepare_resolve_profile",
            job_id=request.job_id,
            cache_hit=False,
            source_count=len(requests_by_source),
            symbol_count=len(requests),
            export_frame_count=export_frame_count,
            total_ms=round((time.perf_counter() - started) * 1000, 1),
            **{key: round(value, 1) for key, value in sorted(timings.items())},
        )
        return {
            "schema_version": 1,
            "job_id": request.job_id,
            "cache_hit": False,
            "input_key": input_key,
            "render_hash": digest,
            "final_key": final_key,
            "sources": [
                {
                    "idx": index,
                    "key": source_records[source].key,
                    "sha256": source_records[source].sha256,
                    "requests": [
                        {
                            "key": symbol.key,
                            "class_name": symbol.class_name,
                            "character_id": symbol.character_id,
                            "frame": symbol.frame,
                        }
                        for symbol in sorted(group, key=lambda item: item.key)
                    ],
                }
                for index, (source, group) in enumerate(
                    sorted(requests_by_source.items(), key=lambda item: str(item[0]))
                )
            ],
        }


def prepare_export_source(
    *,
    job_id: str,
    input_key: str,
    source: Mapping[str, Any],
    store: StageStore,
    config: RuntimeConfig,
) -> dict[str, Any]:
    """Phase 2: export one source SWF's symbol timelines in parallel.

    Downloads prepare-input.json + its source SWF, runs FFDec once for that
    source, uploads one per-symbol tar.gz archive, and reports the parts plus
    color rules and authored placement colors computed from the local file.
    """
    started = time.perf_counter()
    prepared = store.read_json(config.work_bucket, input_key)
    if prepared.get("job_id") != job_id:
        raise character_svg.CharacterSvgError("Prepare input belongs to another job")
    source_idx = int(source["idx"])
    settings = prepared["settings"]
    zoom = float(settings["zoom"])
    export_frame_count = int(prepared["export_frame_count"])
    subframe_start = int(settings["subframe_start"])

    with tempfile.TemporaryDirectory(prefix=f"aqw-export-{job_id}-{source_idx}-") as temporary:
        root = Path(temporary)
        swf = store.download(
            config.source_bucket,
            str(source["key"]),
            root / "source.swf",
            expected_sha256=str(source["sha256"]),
        )
        requests = [
            character_svg.SymbolRequest(
                key=str(request["key"]),
                source=swf,
                class_name=str(request["class_name"]),
                character_id=int(request["character_id"]),
                frame=int(request["frame"]),
            )
            for request in source["requests"]
        ]
        exported = character_svg.export_requested_symbol_frames(
            requests,
            ffdec=config.ffdec_path,
            zoom=zoom,
            destination=root / "exports",
            subframe_start=subframe_start,
            frame_count=export_frame_count,
        )
        record = SourceObject(
            remote_path=str(source.get("remote_path") or ""),
            key=str(source["key"]),
            sha256=str(source["sha256"]),
            size=0,
        )
        rules = _source_color_rules(
            swf,
            record,
            store=store,
            config=config,
            scripts_root=root / "scripts",
        )
        placement_colors = character_svg.authored_swf_color_transforms(swf)

        archive_directory = root / "archives"
        archive_directory.mkdir(parents=True, exist_ok=True)
        parts: list[dict[str, Any]] = []
        archive_bytes = 0
        batch_size = config.batch_size
        for symbol in sorted(requests, key=lambda item: item.key):
            frames = exported[symbol.key]
            # Each batch worker only consumes its own frame range plus the
            # overlap frame, so split each symbol into per-batch archives:
            # a probe/raster worker then downloads a few hundred KB instead
            # of the whole 100+ MiB corpus.
            chunked: dict[int, str] = {}
            for batch_index in range(0, len(frames), batch_size):
                chunk = frames[batch_index:batch_index + batch_size]
                archive_path = archive_directory / f"{symbol.key}.{len(chunked)}.tar.gz"
                with tarfile.open(archive_path, "w:gz", compresslevel=1) as archive:
                    for index, path in enumerate(chunk, start=batch_index + 1):
                        archive.add(path, arcname=f"{index:06d}.svg")
                archive_bytes += archive_path.stat().st_size
                archive_key = f"jobs/{job_id}/prepare/parts/{symbol.key}.{len(chunked)}.tar.gz"
                store.upload_file(
                    archive_path,
                    config.work_bucket,
                    archive_key,
                    content_type="application/gzip",
                )
                chunked[batch_index // batch_size] = archive_key
            # Keep a full per-symbol archive so the finish phase can rebuild
            # every frame for loop detection and the shared viewbox.
            full_path = archive_directory / f"{symbol.key}.full.tar.gz"
            with tarfile.open(full_path, "w:gz", compresslevel=1) as archive:
                for index, path in enumerate(frames, start=1):
                    archive.add(path, arcname=f"{index:06d}.svg")
            full_key = f"jobs/{job_id}/prepare/parts/{symbol.key}.full.tar.gz"
            store.upload_file(
                full_path,
                config.work_bucket,
                full_key,
                content_type="application/gzip",
            )
            parts.append(
                {
                    "key": symbol.key,
                    "root_class": symbol.class_name,
                    "character_id": symbol.character_id,
                    "archive_key": full_key,
                    "frame_count": len(frames),
                }
            )
    log_event(
        "prepare_export_complete",
        job_id=job_id,
        source_idx=source_idx,
        parts=len(parts),
        archive_bytes=archive_bytes,
        duration_ms=round((time.perf_counter() - started) * 1000),
    )
    return {
        "job_id": job_id,
        "source_idx": source_idx,
        "parts": parts,
        "color_rules": {key: list(value) for key, value in rules.items()},
        "placement_colors": {
            f"{parent_id},{child_id}": {
                field: getattr(transform, field)
                for field in (
                    "red_mult",
                    "green_mult",
                    "blue_mult",
                    "alpha_mult",
                    "red_add",
                    "green_add",
                    "blue_add",
                    "alpha_add",
                )
            }
            for (parent_id, child_id), transform in sorted(placement_colors.items())
        },
    }


def prepare_finish(
    *,
    request: JobRequest,
    input_key: str,
    export_results: list[dict[str, Any]],
    store: StageStore,
    config: RuntimeConfig,
) -> dict[str, Any]:
    """Phase 3: rebuild exports, detect loop, compute viewbox, write manifest."""
    started = time.perf_counter()
    timings: dict[str, float] = {}

    def mark(name: str, phase: float) -> None:
        timings[name] = timings.get(name, 0.0) + (time.perf_counter() - phase) * 1000

    phase = time.perf_counter()
    prepared = store.read_json(config.work_bucket, input_key)
    if prepared.get("job_id") != request.job_id:
        raise character_svg.CharacterSvgError("Prepare input belongs to another job")
    mark("input_read_ms", phase)

    # Rebuild per-symbol raw exports from the archives the export Lambdas
    # uploaded, keyed for the loop detector and viewbox math.
    with tempfile.TemporaryDirectory(prefix=f"aqw-finish-{request.job_id}-") as temporary:
        root = Path(temporary)
        export_key_to_result = {int(result["source_idx"]): result for result in export_results}
        raw_exports: dict[str, list[Path]] = {}
        part_manifest: dict[str, Any] = {}
        all_color_rules: set[tuple[str, str]] = set()
        for result in sorted(export_key_to_result.values(), key=lambda value: int(value["source_idx"])):
            rules = {
                name: tuple(rule) for name, rule in result["color_rules"].items()
            }
            placement = {
                tuple(int(component) for component in pair.split(",")):
                character_svg.AuthoredColorTransform(**values)
                for pair, values in result["placement_colors"].items()
            }
            for part in result["parts"]:
                phase = time.perf_counter()
                archive_path = store.download(
                    config.work_bucket,
                    part["archive_key"],
                    root / "archives" / f"{part['key']}.tar.gz",
                )
                target = root / "parts" / part["key"]
                _extract_archive(archive_path, target)
                mark("archive_download_ms", phase)
                phase = time.perf_counter()
                frames = sorted(
                    target.glob("*.svg"),
                    key=lambda path: int(path.stem) if path.stem.isdigit() else 0,
                )
                raw_exports[part["key"]] = frames
                part_manifest[part["key"]] = {
                    "root_class": part["root_class"],
                    "character_id": part["character_id"],
                    "archive_key": part["archive_key"],
                    # Rebuild the per-batch archive keys deterministically so
                    # the ~7000-entry map never travels through Step Functions
                    # state; workers resolve their slice by ordinal.
                    "batch_archives": {
                        str(ordinal): (
                            f"jobs/{request.job_id}/prepare/parts/"
                            f"{part['key']}.{ordinal}.tar.gz"
                        )
                        for ordinal in range(
                            (part["frame_count"] + config.batch_size - 1)
                            // config.batch_size
                        )
                    },
                    "frame_count": part["frame_count"],
                    "color_rules": {
                        key: list(value)
                        for key, value in sorted(rules.items())
                    },
                    "placement_colors": {
                        f"{parent_id},{child_id}": {
                            field: getattr(transform, field)
                            for field in (
                                "red_mult",
                                "green_mult",
                                "blue_mult",
                                "alpha_mult",
                                "red_add",
                                "green_add",
                                "blue_add",
                                "alpha_add",
                            )
                        }
                        for (parent_id, child_id), transform in sorted(placement.items())
                    },
                }
                all_color_rules.update(
                    tuple(rule) for rule in rules.values()
                )
                mark("archive_extract_ms", phase)

        phase = time.perf_counter()
        detection_exports = {
            key: frames[: int(prepared["settings"]["max_frames"])]
            for key, frames in raw_exports.items()
        }
        detected_loop: int | None = None
        detected_item_loop: int | None = None
        detected_blink_frames: int | None = None
        ignored_loop_keys: tuple[str, ...] = ()
        warnings = list(prepared.get("warnings") or [])
        if request.render.complete_loop:
            loop_exports, ignored_loop_keys = character_svg.loop_driver_exports(
                detection_exports
            )
            detected_item_loop = character_svg.detect_complete_loop_frame_count(
                loop_exports, max_frames=request.render.max_frames
            )
            detected_blink_frames = character_svg.detect_blink_frame_count(
                detection_exports,
                max_frames=request.render.max_frames,
            )
            if detected_item_loop is not None and detected_blink_frames is not None:
                detected_loop = character_svg.aligned_animation_frame_count(
                    detected_item_loop,
                    detected_blink_frames,
                )
                frame_count = min(detected_loop, request.render.max_frames)
            else:
                frame_count = request.render.max_frames
            if detected_item_loop is None:
                warnings.append(
                    "At least one nested item timeline did not repeat within the frame cap"
                )
            if detected_blink_frames is None:
                warnings.append(
                    "The natural eye-blink timeline did not repeat within the frame cap"
                )
        else:
            frame_count = 1
        mark("loop_detection_ms", phase)

        phase = time.perf_counter()
        layers = character_svg.build_layers(prepared["aliases"], weapon_type=prepared["weapon_type"])
        viewbox = shared_viewbox(
            layers,
            raw_exports,
            frame_count=frame_count,
            facing=request.render.facing,
            zoom=float(prepared["settings"]["zoom"]),
            max_size=int(prepared["settings"]["max_size"]),
            padding=int(prepared["settings"]["padding"]),
        )
        mark("viewbox_ms", phase)

        phase = time.perf_counter()
        character_renderer = store.download(
            config.source_bucket,
            prepared["character_renderer"]["key"],
            root / "characterB.swf",
            expected_sha256=prepared["character_renderer"]["sha256"],
        )
        frame_rate = character_svg.swf_frame_rate(character_renderer)
        batches = [batch.to_dict() for batch in partition_frames(frame_count, config.batch_size)]
        manifest_key = f"jobs/{request.job_id}/prepare/manifest.json"
        manifest = {
            "schema_version": 1,
            "job_id": request.job_id,
            "render_hash": prepared["render_hash"],
            "final_key": prepared["final_key"],
            "frame_count": frame_count,
            "frame_rate": frame_rate,
            "viewbox": list(viewbox),
            "frame_durations": character_svg.frame_durations_for_rate(frame_count, frame_rate),
            "fields": dict(sorted(prepared["fields"].items())),
            "aliases": dict(sorted(prepared["aliases"].items())),
            "weapon_type": prepared["weapon_type"],
            "parts": part_manifest,
            "all_color_rules": sorted(all_color_rules),
            "settings": request.render.to_dict(),
            "batches": batches,
            "warnings": warnings,
            "detected_loop": detected_loop,
            "detected_blink_frames": detected_blink_frames,
            "ignored_loop_keys": list(ignored_loop_keys),
            "sources": [],
        }
        store.write_json(config.work_bucket, manifest_key, manifest)
        mark("manifest_write_ms", phase)

        if request.render.complete_loop:
            natural_loop = (
                character_svg.aligned_animation_frame_count(
                    detected_item_loop, detected_blink_frames
                )
                if detected_item_loop is not None and detected_blink_frames is not None
                else None
            )
            loop_capped = frame_count < natural_loop if natural_loop is not None else True
            symbol_loops = character_svg.symbol_loop_info(
                detection_exports,
                max_frames=request.render.max_frames,
                validation_frames=character_svg.LOOP_VALIDATION_FRAMES,
            )
        else:
            natural_loop = None
            loop_capped = False
            symbol_loops = {}
        total_ms = (time.perf_counter() - started) * 1000
        accounted = sum(timings.values())
        log_event(
            "prepare_profile",
            job_id=request.job_id,
            frame_count=frame_count,
            export_frame_count=int(prepared["export_frame_count"]),
            detected_item_loop=detected_item_loop,
            detected_blink_frames=detected_blink_frames,
            natural_loop=natural_loop,
            loop_capped=loop_capped,
            symbol_loops=symbol_loops,
            symbol_count=len(part_manifest),
            total_ms=round(total_ms, 1),
            unaccounted_ms=round(total_ms - accounted, 1),
            **{key: round(value, 1) for key, value in sorted(timings.items())},
        )
        return {
            "schema_version": 1,
            "job_id": request.job_id,
            "cache_hit": False,
            "render_hash": prepared["render_hash"],
            "final_key": prepared["final_key"],
            "manifest_key": manifest_key,
            "batches": batches,
            "frame_count": frame_count,
        }