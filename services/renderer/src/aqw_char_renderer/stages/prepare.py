"""Resolve one appearance, export FFDec parts, detect its loop, and write a manifest."""

from __future__ import annotations

import math
import re
import tarfile
import tempfile
from collections.abc import Mapping, Sequence
from dataclasses import asdict
from pathlib import Path
from typing import Any, Protocol

from aqw_char_renderer import character_svg
from aqw_char_renderer.batching import partition_frames
from aqw_char_renderer.config import RuntimeConfig
from aqw_char_renderer.contracts import JobRequest
from aqw_char_renderer.geometry import shared_canvas, union_bounds
from aqw_char_renderer.hashing import canonical_sha256, render_key
from aqw_char_renderer.legacy import preview_aqw_tryon as tryon
from aqw_char_renderer.legacy import render_swf_items as item_renderer
from aqw_char_renderer.source_assets import SourceAssetCatalog, SourceObject


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


def prepare_job(
    request: JobRequest,
    *,
    store: StageStore,
    config: RuntimeConfig,
    flashvars: Mapping[str, str] | None = None,
    work_root: Path | None = None,
) -> dict[str, Any]:
    """Execute the expensive serial preparation stage for one job."""
    manifest_payload = store.read_json(config.source_bucket, config.asset_manifest_key)
    catalog = SourceAssetCatalog(manifest_payload)
    if catalog.dataset_version != config.asset_dataset_version:
        raise character_svg.CharacterSvgError(
            "Configured asset dataset does not match its source manifest"
        )

    owned_temporary: tempfile.TemporaryDirectory[str] | None = None
    if work_root is None:
        owned_temporary = tempfile.TemporaryDirectory(prefix=f"aqw-prepare-{request.job_id}-")
        root = Path(owned_temporary.name)
    else:
        root = work_root
        root.mkdir(parents=True, exist_ok=True)
    try:
        fields = (
            {str(key): str(value) for key, value in flashvars.items()}
            if flashvars is not None
            else tryon.fetch_character_flashvars(request.render.username, timeout=15)
        )
        if request.render.show_hidden:
            fields["ia1"] = str(character_svg._visibility_flags(fields) & ~0b111)

        asset_root = root / "assets"
        database = store.download(
            config.source_bucket,
            catalog.item_database.key,
            root / "item_db.json",
            expected_sha256=catalog.item_database.sha256,
        )
        explicit, source_records = _override(
            request, fields, catalog, store, config, asset_root, database
        )
        assets = character_svg.appearance_assets(
            fields, use_cosmetics=not request.render.base_items
        )
        sources: dict[str, Path] = {}
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
        cached = store.exists(config.work_bucket, final_key)
        if cached is not None:
            metadata = dict(cached.get("Metadata") or {})
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
        raw_exports = character_svg.export_requested_symbol_frames(
            requests,
            ffdec=config.ffdec_path,
            zoom=request.render.zoom,
            destination=root / "exports",
            subframe_start=request.render.subframe_start,
            frame_count=export_frame_count,
        )
        detected_loop: int | None = None
        ignored_loop_keys: tuple[str, ...] = ()
        if request.render.complete_loop:
            loop_exports, ignored_loop_keys = character_svg.loop_driver_exports(raw_exports)
            detected_loop = character_svg.detect_complete_loop_frame_count(
                loop_exports, max_frames=request.render.max_frames
            )
            frame_count = (
                min(detected_loop, request.render.max_frames)
                if detected_loop
                else request.render.max_frames
            )
            if detected_loop is None:
                warnings.append("At least one nested timeline did not repeat within the frame cap")
        else:
            frame_count = 1

        rules_by_source: dict[Path, dict[str, tuple[str, str]]] = {}
        for source in sorted({symbol.source for symbol in requests}):
            rules_by_source[source] = character_svg.parse_color_scripts(
                source,
                ffdec=config.ffdec_path,
                destination=root / "scripts",
            )

        layers = character_svg.build_layers(aliases, weapon_type=weapon_type)
        viewbox = shared_viewbox(
            layers,
            raw_exports,
            frame_count=frame_count,
            facing=request.render.facing,
            zoom=request.render.zoom,
            max_size=request.render.max_size,
            padding=request.render.padding,
        )

        # Upload one compressed archive per symbol part instead of one object
        # per frame: downstream workers fetch a handful of archives rather
        # than thousands of individual SVGs.
        archive_directory = root / "archives"
        archive_directory.mkdir(parents=True, exist_ok=True)
        part_manifest: dict[str, Any] = {}
        for symbol in requests:
            frames = raw_exports[symbol.key][:frame_count]
            archive_path = archive_directory / f"{symbol.key}.tar.gz"
            with tarfile.open(archive_path, "w:gz") as archive:
                for index, path in enumerate(frames, start=1):
                    archive.add(path, arcname=f"{index:06d}.svg")
            archive_key = f"jobs/{request.job_id}/prepare/parts/{symbol.key}.tar.gz"
            store.upload_file(
                archive_path,
                config.work_bucket,
                archive_key,
                content_type="application/gzip",
            )
            part_manifest[symbol.key] = {
                "root_class": symbol.class_name,
                "archive_key": archive_key,
                "frame_count": len(frames),
                "color_rules": {
                    key: list(value)
                    for key, value in sorted(rules_by_source.get(symbol.source, {}).items())
                },
            }

        frame_rate = character_svg.swf_frame_rate(character_renderer)
        batches = [batch.to_dict() for batch in partition_frames(frame_count, config.batch_size)]
        manifest_key = f"jobs/{request.job_id}/prepare/manifest.json"
        manifest = {
            "schema_version": 1,
            "job_id": request.job_id,
            "render_hash": digest,
            "final_key": final_key,
            "frame_count": frame_count,
            "frame_rate": frame_rate,
            "viewbox": list(viewbox),
            "frame_durations": character_svg.frame_durations_for_rate(frame_count, frame_rate),
            "fields": dict(sorted(fields.items())),
            "aliases": dict(sorted(aliases.items())),
            "weapon_type": weapon_type,
            "parts": part_manifest,
            "all_color_rules": sorted(
                {
                    tuple(rule)
                    for part in part_manifest.values()
                    for rule in part["color_rules"].values()
                }
            ),
            "settings": request.render.to_dict(),
            "batches": batches,
            "warnings": warnings,
            "detected_loop": detected_loop,
            "ignored_loop_keys": list(ignored_loop_keys),
            "sources": [
                {
                    "remote_path": record.remote_path,
                    "key": record.key,
                    "sha256": record.sha256,
                    "size": record.size,
                }
                for record in sorted(source_records.values(), key=lambda value: value.key)
            ],
        }
        store.write_json(config.work_bucket, manifest_key, manifest)
        return {
            "schema_version": 1,
            "job_id": request.job_id,
            "cache_hit": False,
            "render_hash": digest,
            "final_key": final_key,
            "manifest_key": manifest_key,
            "batches": batches,
            "frame_count": frame_count,
        }
    finally:
        if owned_temporary is not None:
            owned_temporary.cleanup()
