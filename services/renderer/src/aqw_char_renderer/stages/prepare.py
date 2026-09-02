"""Prepare phases: resolve appearance, export FFDec parts per source, finish manifest.

Split across three Lambda phases so the expensive per-source FFDec export runs
in parallel (one Lambda per source SWF) instead of serially inside a single
prepare invocation:

- prepare_resolve: resolve appearance, build symbol requests, hash, cache
  check, and publish prepare-input.json.
- prepare_export_source: one source SWF, reuse or export its symbol timelines,
  publish per-source frame bundles, metadata, color rules, and placements.
- prepare_finish: read the compact metadata, detect the loop, compute the
  shared viewbox, and write the render manifest.
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

import numpy as np
from botocore.exceptions import ClientError

from aqw_char_renderer import character_svg
from aqw_char_renderer.batching import partition_frames
from aqw_char_renderer.config import RuntimeConfig
from aqw_char_renderer.contracts import JobRequest
from aqw_char_renderer.geometry import shared_canvas, union_bounds
from aqw_char_renderer.hashing import canonical_sha256, file_sha256, render_key
from aqw_char_renderer.legacy import preview_aqw_tryon as tryon
from aqw_char_renderer.legacy import render_swf_items as item_renderer
from aqw_char_renderer.source_assets import SourceAssetCatalog, SourceObject
from aqw_char_renderer.storage import StorageError
from aqw_char_renderer.structured_logging import log_event

FFDEC_VERSION = "26.2.1"
ANIMATION_METADATA_SCHEMA = 2
# v5: cache identity includes whether the exported symbol advances its own
# root timeline instead of only nested subframes.
# v4: states that rasterize with zero visible pixels (e.g. opacity-0 blink
# frames in an animated cape) store a null bound instead of the loose header
# canvas. The old v3 behavior let one invisible state's full declared sprite
# stage inflate the shared viewbox, leaving large empty margins.
VECTOR_CACHE_SCHEMA = 5


class _InvisibleState:
    """Sentinel returned when a state rasterizes cleanly but has no pixels."""


_INVISIBLE_STATE = _InvisibleState()


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
    def upload_file_if_absent(self, source: Path, bucket: str, key: str, **kwargs: Any) -> bool: ...
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


def _source_terminal_stops(
    source: Path,
    record: SourceObject,
    *,
    store: StageStore,
    config: RuntimeConfig,
    scripts_root: Path,
) -> dict[str, int]:
    """Authored AS3 stop frames for one immutable SWF, cached by hash."""
    cache_key = f"timeline-stops/1/{record.sha256}.json"
    try:
        payload = store.read_json(config.work_bucket, cache_key)
        if isinstance(payload, Mapping):
            return {str(name): int(frame) for name, frame in payload.items()}
    except ClientError as error:
        if error.response.get("Error", {}).get("Code") not in {
            "NoSuchKey",
            "NoSuchBucket",
            "404",
        }:
            log_event("timeline_stops_cache_read_failed", key=cache_key, error=str(error))
    except (StorageError, TypeError, ValueError, json.JSONDecodeError, OSError) as error:
        log_event("timeline_stops_cache_corrupt", key=cache_key, error=str(error))
    computed = character_svg.parse_terminal_stop_frames(
        source,
        ffdec=config.ffdec_path,
        destination=scripts_root,
    )
    try:
        store.write_json(config.work_bucket, cache_key, computed)
    except (StorageError, ClientError, OSError) as error:
        log_event("timeline_stops_cache_write_failed", key=cache_key, error=str(error))
    return computed


_SVG_ROOT_TAG_RE = re.compile(rb"<svg\b[^>]*>", re.DOTALL)
_MATRIX_ATTR_RE = re.compile(rb'transform="(matrix\([^)]*\))"')


def _svg_dimension(root_tag: bytes, name: str) -> float | None:
    match = re.search(rb"\b" + name.encode("ascii") + rb'="([^"]*)"', root_tag)
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
        body = data[:defs_start] + data[defs_end + len(b"</defs>") :]
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
    # Zero margin: the per-state alpha probes already pin the tight visible
    # bounds with 1px padding, so no additional page is needed. Clipping a
    # filter glow is an accepted trade-off for a fully-filled frame (the old
    # 10%% margin just expanded the canvas into empty bands).
    margin = 0.0
    return shared_canvas(
        [(tight[0] - margin, tight[1] - margin, tight[2] + margin * 2, tight[3] + margin * 2)],
        max_size=max_size,
        padding=padding,
    )


def shared_viewbox_from_bounds(
    layers: Sequence[character_svg.Layer],
    symbol_bounds: Mapping[str, Sequence[tuple[float, float, float, float] | None]],
    *,
    frame_count: int,
    facing: str,
    zoom: float,
    max_size: int,
    padding: int,
) -> tuple[float, float, float, float]:
    """shared_viewbox from precomputed per-frame header bounds (no SVG reads)."""
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
        for bounds in symbol_bounds.get(layer.symbol_key, [])[:frame_count]:
            if bounds is None:
                continue
            transformed.append(character_svg._transformed_bounds(bounds, matrix))
    try:
        ans = union_bounds(transformed)
    except ValueError as error:
        raise character_svg.CharacterSvgError(
            "Character composition produced no visible layers"
        ) from error
    # Zero margin: per-state alpha probes are exact (1px padding), so the
    # garbage 10%% cushion is removed entirely (glow clipping accepted).
    margin = 0.0
    return shared_canvas(
        [(ans[0] - margin, ans[1] - margin, ans[2] + margin * 2, ans[3] + margin * 2)],
        max_size=max_size,
        padding=padding,
    )


def _extract_archive(archive_path: Path, target: Path) -> None:
    target.mkdir(parents=True, exist_ok=True)
    with tarfile.open(archive_path) as archive:
        archive.extractall(target, filter="data")


def _vector_symbol_identity(request: character_svg.SymbolRequest) -> str:
    """Stable cache identity for one selected SWF symbol/root frame."""
    return canonical_sha256(
        {
            "character_id": request.character_id,
            "class_name": request.class_name,
            "root_frame": request.frame,
            "root_timeline_frames": request.root_timeline_frames,
        }
    )[:24]


def _vector_cache_key(
    record: SourceObject,
    requests: Sequence[character_svg.SymbolRequest],
    *,
    zoom: float,
    subframe_start: int,
    frame_count: int,
) -> str:
    """Content/settings-addressed key for an exact reusable vector export."""
    request_digest = canonical_sha256(
        [
            {
                "character_id": request.character_id,
                "class_name": request.class_name,
                "root_frame": request.frame,
                "root_timeline_frames": request.root_timeline_frames,
            }
            for request in sorted(
                requests,
                key=lambda value: (
                    value.character_id,
                    value.frame,
                    value.class_name,
                ),
            )
        ]
    )
    zoom_label = f"{zoom:.12g}"
    return (
        f"vector-states/{VECTOR_CACHE_SCHEMA}/{FFDEC_VERSION}/z{zoom_label}/"
        f"start-{subframe_start}/frames-{frame_count}/{record.sha256}/"
        f"{request_digest}.tar.gz"
    )


def _validated_cached_bounds(
    value: Any,
) -> tuple[float, float, float, float] | None:
    if value is None:
        return None
    if not isinstance(value, list) or len(value) != 4:
        raise character_svg.CharacterSvgError("Vector cache has invalid frame bounds")
    parsed = tuple(float(component) for component in value)
    if not all(math.isfinite(component) for component in parsed):
        raise character_svg.CharacterSvgError("Vector cache has non-finite frame bounds")
    if parsed[2] < 0 or parsed[3] < 0:
        raise character_svg.CharacterSvgError("Vector cache has negative frame bounds")
    return parsed


def _svg_frame_author_invisible(data: bytes) -> bool:
    """Cheap structural check for authored invisible states.

    Animated AQW parts carry "blink" frames where every element is drawn at
    opacity 0 (the timeline pauses a beat). The opacity attributes live inside
    the nested sprite ``<defs>`` (the rendered wrapper only ``<use>``s them),
    so scan the whole document: when every explicit opacity attribute is zero
    the frame paints nothing and contributes nothing to the shared canvas.
    """
    text = data.decode("utf-8", errors="replace")
    opacities = re.findall(r"opacity=\"([^\"]+)\"", text)
    if not opacities:
        return False
    for value in opacities:
        try:
            if float(value.strip()) != 0.0:
                return False
        except ValueError:
            return False
    return True


def _probe_state_bounds(
    path: Path,
    zoom: float,
    rsvg_convert: str,
) -> tuple[float, float, float, float] | None | _InvisibleState:
    """Alpha-probe one exported state; return tight bounds in registration space.

    ``_INVISIBLE_STATE`` is returned when the state has no visible pixels by
    construction (e.g. an opacity-0 blink frame in an animated cape): such a
    state contributes nothing to the rendered animation, so it must also
    contribute nothing to the shared canvas. Detection is cheap: one 512px
    probe plus a structural opacity scan. ``None`` (probe/matrix failure or
    ambiguous hairline geometry) keeps the caller's conservative header-canvas
    fallback.
    """
    data = path.read_bytes()
    body = data
    defs_start = data.find(b"<defs")
    defs_end = data.find(b"</defs>")
    if 0 <= defs_start < defs_end:
        body = data[:defs_start] + data[defs_end + len(b"</defs>") :]
    matrix_match = _MATRIX_ATTR_RE.search(body)
    matrix = character_svg.parse_matrix(
        matrix_match.group(1).decode("utf-8", "replace") if matrix_match else None
    )
    if matrix is None:
        return None
    a, _b, _c, d, e, f = matrix
    # FFDec is asked to export at ``zoom``, but some sprites (e.g. DACE capes)
    # ignore -zoom and export at their native scale with an identity wrapper.
    # Normalize the wrapper scale so the registration math stays consistent
    # regardless of whether the export actually applied zoom.
    if abs(a - d) > 1e-4:
        return None
    frame_scale = (a + d) / 2.0
    if frame_scale <= 0 or not math.isfinite(frame_scale):
        return None
    if not (abs(frame_scale - zoom) < 1e-3 or abs(frame_scale - 1.0) < 1e-3):
        return None
    # Probe twice, bounded at 1024, so hairline content that aliases away at
    # 512 is still caught while invisible blink states cost only two small
    # renders (never the 2048 raster that previously blew the 900s export
    # budget on flicker-heavy weapons/capes).
    for probe_size in (512, 1024):
        probe = item_renderer.probe_svg_alpha(path, rsvg_convert, probe_size=probe_size)
        if probe is None:
            # probe_svg_alpha collapses both "rasterized empty" and a render
            # failure into None; keep trying the next size.
            continue
        canvas, alpha = probe
        if not np.any(alpha):
            continue
        probed = item_renderer.visible_viewbox_from_probe(
            (canvas, alpha),
            padding_pixels=1,
        )
        if probed is None:
            continue
        x, y, width, height = probed
        return (
            (x - e) / frame_scale,
            (y - f) / frame_scale,
            width / frame_scale,
            height / frame_scale,
        )
    # No pixels at either probe size. If the frame is authored invisible
    # (every element's opacity is 0), mark it so without any extra
    # rasterization; that is the AQW blink/rest pattern that previously
    # inflated the shared canvas via the loose header fallback.
    if _svg_frame_author_invisible(data):
        return _INVISIBLE_STATE
    return None


def _export_metadata(
    paths: Sequence[Path],
    *,
    zoom: float,
    config: RuntimeConfig,
) -> tuple[list[str], list[tuple[float, float, float, float] | None]]:
    """Hash and bound an effective timeline produced after cache loading."""
    signatures: list[str] = []
    bounds: list[tuple[float, float, float, float] | None] = []
    bounds_by_signature: dict[str, tuple[float, float, float, float] | None] = {}
    for path in paths:
        signature = file_sha256(path)
        if signature not in bounds_by_signature:
            probed = _probe_state_bounds(path, zoom, config.rsvg_convert)
            if probed is _INVISIBLE_STATE:
                visible_bounds = None
            elif probed is None:
                try:
                    visible_bounds = export_frame_bounds(path, zoom)
                except character_svg.CharacterSvgError:
                    visible_bounds = None
            else:
                visible_bounds = probed
            bounds_by_signature[signature] = visible_bounds
        signatures.append(signature)
        bounds.append(bounds_by_signature[signature])
    return signatures, bounds


def _load_vector_cache(
    archive_path: Path,
    root: Path,
    record: SourceObject,
    requests: Sequence[character_svg.SymbolRequest],
    *,
    zoom: float,
    subframe_start: int,
    frame_count: int,
) -> tuple[
    dict[str, list[Path]],
    dict[str, list[str]],
    dict[str, list[tuple[float, float, float, float] | None]],
]:
    """Load and fully validate one immutable unique-vector-state archive."""
    _extract_archive(archive_path, root)
    try:
        payload = json.loads((root / "manifest.json").read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as error:
        raise character_svg.CharacterSvgError(
            f"Vector cache has no valid manifest: {error}"
        ) from error
    if not isinstance(payload, Mapping) or payload.get("schema_version") != VECTOR_CACHE_SCHEMA:
        raise character_svg.CharacterSvgError("Vector cache schema is unsupported")
    expected = {
        "ffdec_version": FFDEC_VERSION,
        "swf_sha256": record.sha256,
        "zoom": zoom,
        "subframe_start": subframe_start,
        "frame_count": frame_count,
    }
    for field, value in expected.items():
        if payload.get(field) != value:
            raise character_svg.CharacterSvgError(
                f"Vector cache {field} does not match this export"
            )
    raw_symbols = payload.get("symbols")
    if not isinstance(raw_symbols, Mapping):
        raise character_svg.CharacterSvgError("Vector cache has no symbols mapping")

    exported: dict[str, list[Path]] = {}
    signatures: dict[str, list[str]] = {}
    bounds: dict[str, list[tuple[float, float, float, float] | None]] = {}
    resolved_root = root.resolve()
    for request in requests:
        identity = _vector_symbol_identity(request)
        raw_symbol = raw_symbols.get(identity)
        if not isinstance(raw_symbol, Mapping):
            raise character_svg.CharacterSvgError(
                f"Vector cache is missing symbol {request.class_name}"
            )
        if (
            raw_symbol.get("class_name") != request.class_name
            or raw_symbol.get("character_id") != request.character_id
            or raw_symbol.get("root_frame") != request.frame
            or raw_symbol.get("root_timeline_frames", 1) != request.root_timeline_frames
        ):
            raise character_svg.CharacterSvgError(
                f"Vector cache symbol metadata does not match {request.class_name}"
            )
        raw_states = raw_symbol.get("states")
        schedule = raw_symbol.get("schedule")
        if not isinstance(raw_states, list) or not raw_states:
            raise character_svg.CharacterSvgError("Vector cache symbol has no states")
        if not isinstance(schedule, list) or len(schedule) != frame_count:
            raise character_svg.CharacterSvgError(
                "Vector cache schedule length does not match this export"
            )
        state_paths: list[Path] = []
        state_signatures: list[str] = []
        state_bounds: list[tuple[float, float, float, float] | None] = []
        for state_id, raw_state in enumerate(raw_states):
            if not isinstance(raw_state, Mapping):
                raise character_svg.CharacterSvgError("Vector cache state is invalid")
            expected_name = f"states/{identity}/{state_id}.svg"
            if raw_state.get("path") != expected_name:
                raise character_svg.CharacterSvgError("Vector cache state path is invalid")
            path = (root / expected_name).resolve()
            if not path.is_relative_to(resolved_root) or not path.is_file():
                raise character_svg.CharacterSvgError("Vector cache state file is missing")
            signature = str(raw_state.get("sha256") or "")
            if len(signature) != 64 or file_sha256(path) != signature:
                raise character_svg.CharacterSvgError("Vector cache state checksum is invalid")
            state_paths.append(path)
            state_signatures.append(signature)
            state_bounds.append(_validated_cached_bounds(raw_state.get("bounds")))
        if not all(
            isinstance(state_id, int)
            and not isinstance(state_id, bool)
            and 0 <= state_id < len(state_paths)
            for state_id in schedule
        ):
            raise character_svg.CharacterSvgError("Vector cache schedule is invalid")
        exported[request.key] = [state_paths[state_id] for state_id in schedule]
        signatures[request.key] = [state_signatures[state_id] for state_id in schedule]
        bounds[request.key] = [state_bounds[state_id] for state_id in schedule]
    return exported, signatures, bounds


def _build_vector_cache(
    root: Path,
    record: SourceObject,
    requests: Sequence[character_svg.SymbolRequest],
    exported: Mapping[str, Sequence[Path]],
    *,
    zoom: float,
    subframe_start: int,
    frame_count: int,
    config: RuntimeConfig,
) -> tuple[
    Path,
    dict[str, list[str]],
    dict[str, list[tuple[float, float, float, float] | None]],
]:
    """Deduplicate raw exports and package their schedule plus reusable metadata."""
    root.mkdir(parents=True, exist_ok=True)
    symbols: dict[str, Any] = {}
    state_files: list[tuple[Path, str]] = []
    signatures: dict[str, list[str]] = {}
    bounds: dict[str, list[tuple[float, float, float, float] | None]] = {}
    for request in requests:
        paths = list(exported.get(request.key) or ())
        if len(paths) != frame_count:
            raise character_svg.CharacterSvgError(
                f"FFDec exported {len(paths)} frames for {request.key}; expected {frame_count}"
            )
        identity = _vector_symbol_identity(request)
        signature_to_state: dict[str, int] = {}
        states: list[dict[str, Any]] = []
        schedule: list[int] = []
        state_bounds: list[tuple[float, float, float, float] | None] = []
        for path in paths:
            signature = file_sha256(path)
            state_id = signature_to_state.get(signature)
            if state_id is None:
                state_id = len(states)
                signature_to_state[signature] = state_id
                # Step 6: probe the state's tight visible bounds once at cache
                # time. Invisible states (opacity-0 blink/cape frames) get a
                # null bound so they never inflate the shared canvas; only a
                # genuine probe/raster failure falls back to the loose header
                # canvas (conservative, keeps the artwork unclipped).
                probed_state = _probe_state_bounds(path, zoom, config.rsvg_convert)
                if probed_state is _INVISIBLE_STATE:
                    visible_bounds = None
                elif probed_state is None:
                    try:
                        visible_bounds = export_frame_bounds(path, zoom)
                    except character_svg.CharacterSvgError:
                        # Some sprites export at native scale with a
                        # non-zoom identity wrapper that export_frame_bounds
                        # rejects; that state adds no reliable extent, so
                        # record null and let the shared canvas ignore it.
                        visible_bounds = None
                else:
                    visible_bounds = probed_state
                cached_path = f"states/{identity}/{state_id}.svg"
                states.append(
                    {
                        "path": cached_path,
                        "sha256": signature,
                        "bounds": (list(visible_bounds) if visible_bounds is not None else None),
                    }
                )
                state_bounds.append(visible_bounds)
                state_files.append((path, cached_path))
            schedule.append(state_id)
        symbols[identity] = {
            "class_name": request.class_name,
            "character_id": request.character_id,
            "root_frame": request.frame,
            "root_timeline_frames": request.root_timeline_frames,
            "schedule": schedule,
            "states": states,
        }
        signatures[request.key] = [states[state_id]["sha256"] for state_id in schedule]
        bounds[request.key] = [state_bounds[state_id] for state_id in schedule]

    manifest = {
        "schema_version": VECTOR_CACHE_SCHEMA,
        "ffdec_version": FFDEC_VERSION,
        "swf_sha256": record.sha256,
        "zoom": zoom,
        "subframe_start": subframe_start,
        "frame_count": frame_count,
        "symbols": symbols,
    }
    manifest_path = root / "vector-cache-manifest.json"
    manifest_path.write_text(
        json.dumps(manifest, ensure_ascii=False, sort_keys=True, separators=(",", ":")),
        encoding="utf-8",
    )
    archive_path = root / "vector-cache.tar.gz"
    with tarfile.open(archive_path, "w:gz", compresslevel=1) as archive:
        archive.add(manifest_path, arcname="manifest.json")
        for source, cached_path in state_files:
            archive.add(source, arcname=cached_path)
    return archive_path, signatures, bounds


def build_component_manifest(
    *,
    job_id: str,
    layers: Sequence[character_svg.Layer],
    symbol_signatures: Mapping[str, Sequence[str]],
    part_manifest: Mapping[str, Any],
    frame_count: int,
    frame_durations: Sequence[int],
    facing: str,
    weapon_type: str,
    viewbox: tuple[float, float, float, float],
    raster_size: int,
    output_size: int,
    fields: Mapping[str, str],
    static_keys: Sequence[str],
    ground_animate: Mapping[str, int],
    detected_blink_frames: int | None,
    ignored_loop_keys: Sequence[str],
    source_bundle_frame_count: int,
    renderer_version: str,
) -> tuple[list[dict[str, Any]], list[dict[str, Any]]]:
    """Build the unique placed-component raster tasks and per-frame layer IDs.

    One task covers one unique **placed component state**: a raw SVG state
    (identified by its content hash) plus one exact layer placement (name,
    complete characterB matrix, darkening) and the job's shared viewbox/pixel
    scale, user colors, weapon/facing behavior, and renderer version. Byte-
    identical states reused across frames share one task. Task IDs are
    deterministic, so retries and re-runs are idempotent.

    Returns ``(tasks, frames)`` where ``frames`` lists every output frame with
    its task IDs in exact back-to-front layer order.
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
    static_set = set(static_keys)
    ignored_set = set(ignored_loop_keys)
    colors = {key: value for key, value in sorted(fields.items()) if key.startswith("intColor")}
    tasks_by_id: dict[str, dict[str, Any]] = {}
    frames: list[dict[str, Any]] = []
    for frame_number in range(1, frame_count + 1):
        layer_ids: list[str] = []
        for layer_index, layer in enumerate(layers):
            key = layer.symbol_key
            signatures = symbol_signatures.get(key)
            part = part_manifest.get(key)
            if signatures is None or part is None:
                continue
            source_idx = int(part["source_idx"])
            span = ground_animate.get(key, 0)
            if span >= 2:
                # Random-pose ground cosmetics bob inside their leading pose
                # span; ping-pong it (mirrors the render stage).
                source_frame = character_svg.pingpong_source_frame_index(
                    frame_number - 1,
                    span=span,
                )
            elif key in static_set:
                source_frame = 1
            elif detected_blink_frames and key in ignored_set and detected_blink_frames > 0:
                zero_based = character_svg.one_shot_source_frame_index(
                    frame_number - 1,
                    one_shot_frames=detected_blink_frames,
                )
                source_frame = zero_based + 1
            else:
                source_frame = frame_number
            if source_frame < 1 or source_frame > len(signatures):
                continue
            state_signature = signatures[source_frame - 1]
            matrix = item_renderer.compose_transforms(outer, layer.transform)
            identity = canonical_sha256(
                {
                    "renderer_version": renderer_version,
                    "raster_size": raster_size,
                    "output_size": output_size,
                    "viewbox": [float(value) for value in viewbox],
                    "facing": facing,
                    "weapon_type": weapon_type,
                    "colors": colors,
                    "symbol_key": key,
                    "layer_name": layer.name,
                    "layer_index": layer_index,
                    "matrix": list(matrix),
                    "darken": bool(layer.darken),
                    "state_signature": state_signature,
                    "part": canonical_sha256(part),
                }
            )
            existing = tasks_by_id.get(identity)
            if existing is None:
                ordinal = (source_frame - 1) // source_bundle_frame_count
                tasks_by_id[identity] = {
                    "task_id": identity,
                    "symbol_key": key,
                    "layer_name": layer.name,
                    "layer_index": layer_index,
                    "matrix": [float(value) for value in matrix],
                    "darken": bool(layer.darken),
                    "bundle_key": (
                        f"jobs/{job_id}/prepare/source-bundles/{source_idx}.{ordinal}.tar.gz"
                    ),
                    "member": f"{key}/{source_frame:06d}.svg",
                    "source_frame": source_frame,
                    "state_signature": state_signature,
                }
            layer_ids.append(identity)
        frames.append(
            {
                "number": frame_number,
                "duration_ms": int(frame_durations[frame_number - 1]),
                "layers": layer_ids,
            }
        )
    return list(tasks_by_id.values()), frames


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
        explicit: dict[str, Path] = {}
        source_records: dict[Path, SourceObject] = {}
        if request.render.override is not None:
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
        else:
            timings["item_db_download_ms"] = 0.0
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
                    "root_timeline_frames": symbol.root_timeline_frames,
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
                "ffdec_version": FFDEC_VERSION,
                "libwebp_version": "1.5.0",
                "asset_dataset_version": config.asset_dataset_version,
                "appearance": appearance_for_hash,
                "settings": request.render.to_dict(),
            }
        )
        final_key = render_key(
            config.renderer_version,
            request.render.webp_quality,
            request.render.output_size,
            digest,
        )
        cached = (
            store.exists(config.work_bucket, final_key) if config.render_cache_enabled else None
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
        # Step 4: consult the offline per-SWF animation manifest so the frame
        # count, item loop, and blink span are known before any vector-state
        # export. When metadata is present (dataset was pre-analyzed), the
        # export Map only needs to produce the exact frame range and finish
        # skips signature scanning entirely.
        manifest_meta: dict[str, Any] | None = None
        try:
            if request.render.complete_loop:
                meta_by_source: dict[Path, Any] = {}
                missing = False
                for source in {symbol.source for symbol in requests}:
                    record = source_records[source]
                    try:
                        manifest_meta = store.read_json(
                            config.source_bucket,
                            f"animation-metadata/{ANIMATION_METADATA_SCHEMA}/"
                            f"{FFDEC_VERSION}/{record.sha256}.json",
                        )
                    except Exception:  # noqa: BLE001 - missing metadata is a miss
                        missing = True
                        break
                    meta_by_source[source] = manifest_meta
                if not missing:
                    # Recover each requested symbol's period from the metadata
                    # keyed by class name, then compute the combined loop.
                    # A ground cosmetic analyzed as a random-pose display
                    # (mirror-flip boundary) is frozen at its initial pose
                    # rather than contributing its pose-cycle to the loop.
                    periods: list[int] = []
                    blink_period: int | None = None
                    static_keys: list[str] = []
                    ground_animate: dict[str, int] = {}
                    for symbol in requests:
                        source_meta = meta_by_source.get(symbol.source)
                        symbol_meta = (
                            (source_meta or {}).get("symbols", {}).get(symbol.class_name.casefold())
                        )
                        if not symbol_meta or symbol_meta.get("period") is None:
                            missing = True
                            break
                        if symbol.key in ("ground", "pet") and (
                            bool(symbol_meta.get("random_pose_as3") or False)
                            or int(symbol_meta.get("mirror_flip_frame") or 0) > 0
                        ):
                            flip_frame = int(symbol_meta.get("mirror_flip_frame") or 0)
                            if flip_frame >= 2:
                                # The leading non-mirrored segment is the
                                # authored bobbing animation (ping-pong);
                                # mirror_flip_frame is 0-based, so the
                                # unflipped span equals it.
                                ground_animate[symbol.key] = flip_frame
                            else:
                                static_keys.append(symbol.key)
                            continue
                        if symbol.key == "armor_head":
                            blink_period = int(symbol_meta["period"])
                        else:
                            periods.append(int(symbol_meta["period"]))
                    if not missing:
                        detected_item_loop = math.lcm(*periods) if periods else 1
                        detected_blink_frames = blink_period
                        if detected_item_loop is not None and detected_blink_frames is not None:
                            detected_loop = character_svg.aligned_animation_frame_count(
                                detected_item_loop, detected_blink_frames
                            )
                            frame_count = min(detected_loop, request.render.max_frames)
                        else:
                            frame_count = request.render.max_frames
                        manifest_meta = {
                            "detected_item_loop": detected_item_loop,
                            "detected_blink_frames": detected_blink_frames,
                            "frame_count": frame_count,
                            "source_count": len(meta_by_source),
                            "static_keys": static_keys,
                            "ground_animate": ground_animate,
                        }
        except Exception:  # noqa: BLE001 - any manifest failure degrades to request-time scan
            manifest_meta = None
        if manifest_meta is not None and manifest_meta.get("frame_count"):
            frame_count = int(manifest_meta["frame_count"])
            export_frame_count = frame_count + min(
                character_svg.LOOP_VALIDATION_FRAMES, frame_count
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
                "precomputed_loop": (
                    manifest_meta
                    if manifest_meta is not None and manifest_meta.get("frame_count")
                    else None
                ),
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
                                "root_timeline_frames": symbol.root_timeline_frames,
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
                            "root_timeline_frames": symbol.root_timeline_frames,
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

    Reuses an exact content-addressed vector-state cache when available,
    otherwise runs FFDec once and publishes that cache. Uploads per-source
    frame bundles plus compact signature/bounds metadata for finish/render.
    """
    started = time.perf_counter()
    timings: dict[str, float] = {}

    def mark(name: str, phase: float) -> None:
        timings[name] = timings.get(name, 0.0) + (time.perf_counter() - phase) * 1000

    phase = time.perf_counter()
    prepared = store.read_json(config.work_bucket, input_key)
    mark("input_read_ms", phase)
    if prepared.get("job_id") != job_id:
        raise character_svg.CharacterSvgError("Prepare input belongs to another job")
    source_idx = int(source["idx"])
    settings = prepared["settings"]
    zoom = float(settings["zoom"])
    export_frame_count = int(prepared["export_frame_count"])
    subframe_start = int(settings["subframe_start"])

    with tempfile.TemporaryDirectory(prefix=f"aqw-export-{job_id}-{source_idx}-") as temporary:
        root = Path(temporary)
        phase = time.perf_counter()
        swf = store.download(
            config.source_bucket,
            str(source["key"]),
            root / "source.swf",
            expected_sha256=str(source["sha256"]),
        )
        mark("source_download_ms", phase)
        record = SourceObject(
            remote_path=str(source.get("remote_path") or ""),
            key=str(source["key"]),
            sha256=str(source["sha256"]),
            size=0,
        )
        requests = [
            character_svg.SymbolRequest(
                key=str(request["key"]),
                source=swf,
                class_name=str(request["class_name"]),
                character_id=int(request["character_id"]),
                frame=int(request["frame"]),
                root_timeline_frames=int(request.get("root_timeline_frames", 1)),
            )
            for request in source["requests"]
        ]
        cache_key = _vector_cache_key(
            record,
            requests,
            zoom=zoom,
            subframe_start=subframe_start,
            frame_count=export_frame_count,
        )
        cache_hit = False
        exported: dict[str, list[Path]] = {}
        vector_signatures: dict[str, list[str]] = {}
        vector_bounds: dict[str, list[tuple[float, float, float, float] | None]] = {}
        phase = time.perf_counter()
        cache_exists = store.exists(config.source_bucket, cache_key) is not None
        mark("vector_cache_head_ms", phase)
        if cache_exists:
            phase = time.perf_counter()
            try:
                cache_archive = store.download(
                    config.source_bucket,
                    cache_key,
                    root / "vector-cache" / "states.tar.gz",
                )
                exported, vector_signatures, vector_bounds = _load_vector_cache(
                    cache_archive,
                    root / "cached-states",
                    record,
                    requests,
                    zoom=zoom,
                    subframe_start=subframe_start,
                    frame_count=export_frame_count,
                )
                cache_hit = True
            except Exception as error:  # noqa: BLE001 - corrupt cache must not fail a render
                log_event(
                    "vector_cache_read_failed",
                    job_id=job_id,
                    source_idx=source_idx,
                    cache_key=cache_key,
                    error=f"{type(error).__name__}: {error}",
                )
                exported = {}
                vector_signatures = {}
                vector_bounds = {}
            mark("vector_cache_read_ms", phase)

        cache_created = False
        if not cache_hit:
            phase = time.perf_counter()
            exported = character_svg.export_requested_symbol_frames(
                requests,
                ffdec=config.ffdec_path,
                zoom=zoom,
                destination=root / "exports",
                subframe_start=subframe_start,
                frame_count=export_frame_count,
            )
            mark("ffdec_export_ms", phase)
            phase = time.perf_counter()
            cache_archive, vector_signatures, vector_bounds = _build_vector_cache(
                root / "cache-build",
                record,
                requests,
                exported,
                zoom=zoom,
                subframe_start=subframe_start,
                frame_count=export_frame_count,
                config=config,
            )
            mark("vector_cache_build_ms", phase)
            phase = time.perf_counter()
            try:
                cache_created = store.upload_file_if_absent(
                    cache_archive,
                    config.source_bucket,
                    cache_key,
                    content_type="application/gzip",
                    metadata={
                        "swf-sha256": record.sha256,
                        "ffdec-version": FFDEC_VERSION,
                    },
                )
            except Exception as error:  # noqa: BLE001 - cache is optional acceleration
                log_event(
                    "vector_cache_write_failed",
                    job_id=job_id,
                    source_idx=source_idx,
                    cache_key=cache_key,
                    error=f"{type(error).__name__}: {error}",
                )
            mark("vector_cache_upload_ms", phase)

        phase = time.perf_counter()
        rules = _source_color_rules(
            swf,
            record,
            store=store,
            config=config,
            scripts_root=root / "scripts",
        )
        terminal_stops = _source_terminal_stops(
            swf,
            record,
            store=store,
            config=config,
            scripts_root=root / "scripts",
        )
        placement_colors = character_svg.authored_swf_color_transforms(swf)
        mark("source_metadata_ms", phase)

        # FFDec advances timelines without running their frame scripts. Begin
        # adjacent root-stop states (for example Drudgen's corrected quest
        # bubble placement) and safe single-child stop states on their authored
        # settled frame, while still advancing clips nested inside that frame.
        settled_timelines: dict[str, character_svg.SettledTimeline] = {}
        for symbol in requests:
            frames = exported.get(symbol.key) or []
            if not frames:
                continue
            settled = character_svg.settled_timeline(symbol, frames[0], terminal_stops)
            if settled is not None:
                settled_timelines[symbol.key] = settled
        if settled_timelines:
            phase = time.perf_counter()
            settled_exports = character_svg.export_requested_symbol_frames(
                [settled.request for settled in settled_timelines.values()],
                ffdec=config.ffdec_path,
                zoom=zoom,
                destination=root / "settled-exports",
                subframe_start=subframe_start,
                frame_count=export_frame_count,
            )
            mark("settled_ffdec_export_ms", phase)
            phase = time.perf_counter()
            for key, settled in settled_timelines.items():
                if settled.parent_placement is None:
                    exported[key] = settled_exports[key]
                    signatures, bounds = _export_metadata(
                        settled_exports[key],
                        zoom=zoom,
                        config=config,
                    )
                    vector_signatures[key] = signatures
                    vector_bounds[key] = bounds
                    continue
                transformed_paths = [
                    character_svg.transform_ffdec_registration(
                        path,
                        root / "settled-transformed" / key / f"{index:06d}.svg",
                        placement=settled.parent_placement,
                        zoom=zoom,
                    )
                    for index, path in enumerate(settled_exports[key], start=1)
                ]
                exported[key] = transformed_paths
                signatures, bounds = _export_metadata(
                    transformed_paths,
                    zoom=zoom,
                    config=config,
                )
                vector_signatures[key] = signatures
                vector_bounds[key] = bounds
            mark("settled_transform_ms", phase)

        archive_directory = root / "archives"
        archive_directory.mkdir(parents=True, exist_ok=True)
        parts: list[dict[str, Any]] = []
        archive_bytes = 0
        bundle_frame_count = config.source_bundle_frame_count
        # One archive per (source, chunk ordinal) containing every symbol of
        # this source, so a batch worker fetches ~1 GET per source instead of
        # ~1 GET per symbol. Entries are namespaced <symbol>/<frame>.svg.
        frame_count = int(prepared["export_frame_count"])
        chunk_count = max(
            1,
            (frame_count + bundle_frame_count - 1) // bundle_frame_count,
        )
        bundle_paths = {
            ordinal: archive_directory / f"source.{ordinal}.tar.gz"
            for ordinal in range(chunk_count)
        }
        phase = time.perf_counter()
        for ordinal, bundle_path in sorted(bundle_paths.items()):
            start = ordinal * bundle_frame_count
            stop = start + bundle_frame_count
            with tarfile.open(bundle_path, "w:gz", compresslevel=1) as archive:
                for symbol in sorted(requests, key=lambda item: item.key):
                    for index, path in enumerate(exported[symbol.key][start:stop], start=start + 1):
                        archive.add(path, arcname=f"{symbol.key}/{index:06d}.svg")
        mark("bundle_build_ms", phase)

        phase = time.perf_counter()
        for symbol in sorted(requests, key=lambda item: item.key):
            frames = exported[symbol.key]
            effective_symbol = (
                settled_timelines[symbol.key].request if symbol.key in settled_timelines else symbol
            )
            meta_key = f"jobs/{job_id}/prepare/meta/{symbol.key}.json"
            # Ground/misc cosmetics (and pets) may be authored as random poses
            # whose timeline mirrors the same display list mid-way. Export that
            # boundary so finish can freeze the layer at its initial pose
            # instead of looping the flip-flop.
            mirror_flip_frame = (
                character_svg.detect_mirror_flip_frame(frames)
                if symbol.key in ("ground", "pet")
                else None
            )
            # Author intent is the definitive signal: does the source SWF's
            # decompiled AS3 freezes on a random pose?
            random_pose = (
                character_svg.decompile_as3_has_random_pose(
                    symbol.source,
                    ffdec=config.ffdec_path,
                    destination=root / "scripts",
                )
                if symbol.key in ("ground", "pet")
                else False
            )
            if symbol.key in ("ground", "pet"):
                # The leading, non-mirrored segment (frames 1..span) is the
                # authored bobbing animation; ping-ponging it keeps the motion
                # without looping the mid-timeline direction flip. Without any
                # detected flip the layer freezes at its initial pose instead.
                # mirror_flip_frame is the 0-based index of the first flipped
                # subframe, so the unflipped span equals that index.
                animated_span = (
                    mirror_flip_frame
                    if mirror_flip_frame is not None and mirror_flip_frame >= 2
                    else (1 if random_pose else None)
                )
            else:
                animated_span = None
            store.write_json(
                config.work_bucket,
                meta_key,
                {
                    "frame_signatures": vector_signatures[symbol.key],
                    "frame_bounds": vector_bounds[symbol.key],
                    "frame_count": len(frames),
                    "mirror_flip_frame": mirror_flip_frame,
                    "random_pose_as3": random_pose,
                    "animated_span": animated_span,
                    "root_timeline_frames": symbol.root_timeline_frames,
                    "settled_stop_frame": (
                        settled_timelines[symbol.key].stop_frame
                        if symbol.key in settled_timelines
                        else None
                    ),
                },
            )
            parts.append(
                {
                    "key": symbol.key,
                    "root_class": effective_symbol.class_name,
                    "character_id": effective_symbol.character_id,
                    "frame_count": len(frames),
                    "root_timeline_frames": symbol.root_timeline_frames,
                    "settled_stop_frame": (
                        settled_timelines[symbol.key].stop_frame
                        if symbol.key in settled_timelines
                        else None
                    ),
                }
            )
        mark("meta_upload_ms", phase)

        # Upload the per-source bundles.
        phase = time.perf_counter()
        for ordinal, bundle_path in sorted(bundle_paths.items()):
            archive_bytes += bundle_path.stat().st_size
            bundle_key = f"jobs/{job_id}/prepare/source-bundles/{source_idx}.{ordinal}.tar.gz"
            store.upload_file(
                bundle_path,
                config.work_bucket,
                bundle_key,
                content_type="application/gzip",
            )
        mark("bundle_upload_ms", phase)
    total_ms = (time.perf_counter() - started) * 1000
    accounted_ms = sum(timings.values())
    log_event(
        "prepare_export_complete",
        job_id=job_id,
        source_idx=source_idx,
        parts=len(parts),
        archive_bytes=archive_bytes,
        cache_hit=cache_hit,
        cache_created=cache_created,
        cache_key=cache_key,
        settled_timelines=len(settled_timelines),
        duration_ms=round(total_ms, 1),
        unaccounted_ms=round(total_ms - accounted_ms, 1),
        **{key: round(value, 1) for key, value in sorted(timings.items())},
    )
    return {
        "job_id": job_id,
        "source_idx": source_idx,
        "vector_cache_hit": cache_hit,
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

    # Load per-symbol metadata (signatures + bounds) computed and uploaded by
    # the export Lambdas, so finish never re-downloads or re-parses the SVGs.
    with tempfile.TemporaryDirectory(prefix=f"aqw-finish-{request.job_id}-") as temporary:
        root = Path(temporary)
        export_key_to_result = {int(result["source_idx"]): result for result in export_results}
        symbol_signatures: dict[str, list[str]] = {}
        symbol_bounds: dict[str, list[tuple[float, float, float, float] | None]] = {}
        mirror_flip_frames: dict[str, int] = {}
        random_pose_as3: dict[str, bool] = {}
        ground_animate: dict[str, int] = {}
        part_manifest: dict[str, Any] = {}
        all_color_rules: set[tuple[str, str]] = set()
        for result in sorted(
            export_key_to_result.values(), key=lambda value: int(value["source_idx"])
        ):
            rules = {name: tuple(rule) for name, rule in result["color_rules"].items()}
            placement = {
                tuple(
                    int(component) for component in pair.split(",")
                ): character_svg.AuthoredColorTransform(**values)
                for pair, values in result["placement_colors"].items()
            }
            for part in result["parts"]:
                phase = time.perf_counter()
                meta = store.read_json(
                    config.work_bucket,
                    f"jobs/{request.job_id}/prepare/meta/{part['key']}.json",
                )
                mark("meta_read_ms", phase)
                phase = time.perf_counter()
                symbol_signatures[part["key"]] = list(meta["frame_signatures"])
                symbol_bounds[part["key"]] = [
                    None if value is None else tuple(float(v) for v in value)
                    for value in meta["frame_bounds"]
                ]
                mirror_flip_frames[part["key"]] = int(meta.get("mirror_flip_frame") or 0)
                random_pose_as3[part["key"]] = bool(meta.get("random_pose_as3") or False)
                animated_span = meta.get("animated_span")
                ground_animate[part["key"]] = int(animated_span) if animated_span is not None else 0
                part_record = {
                    "source_idx": int(result["source_idx"]),
                    "root_class": part["root_class"],
                    "character_id": part["character_id"],
                    "frame_count": part["frame_count"],
                    "root_timeline_frames": int(part.get("root_timeline_frames", 1)),
                    "color_rules": {key: list(value) for key, value in sorted(rules.items())},
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
                if part.get("settled_stop_frame"):
                    part_record["settled_stop_frame"] = int(part["settled_stop_frame"])
                # Legacy manifests may still need the full per-symbol archive;
                # new complete source bundles make that duplicate unnecessary.
                if part.get("archive_key"):
                    part_record["archive_key"] = part["archive_key"]
                part_manifest[part["key"]] = part_record
                all_color_rules.update(tuple(rule) for rule in rules.values())
                mark("meta_parse_ms", phase)

        phase = time.perf_counter()
        detection_sigs = symbol_signatures
        detected_loop: int | None = None
        detected_item_loop: int | None = None
        detected_blink_frames: int | None = None
        ignored_loop_keys: tuple[str, ...] = ()
        # Ground/misc cosmetics can be authored as random poses (the display
        # list is mirrored mid-timeline, or the AS3 freezes on a random pose).
        # The leading non-mirrored segment is the authored bobbing animation:
        # ping-pong frames 1..span keeps the up/down motion without looping the
        # direction swap. Layers with no such segment freeze at their initial
        # pose. The decompiled AS3 intent is the definitive signal; the
        # structural mirror-flip detection is the fallback for SWFs where
        # FFDec cannot cleanly decompile the timeline.
        static_keys = tuple(
            key
            for key, flip_frame in mirror_flip_frames.items()
            if key in ("ground", "pet")
            and (random_pose_as3.get(key) or flip_frame > 0)
            and ground_animate.get(key, 0) < 2
        )
        ground_animate = {
            key: span
            for key, span in ground_animate.items()
            if key in ("ground", "pet") and span >= 2
        }
        warnings = list(prepared.get("warnings") or [])
        if static_keys:
            warnings.append(
                "Froze "
                + ", ".join(static_keys)
                + " layer(s) at their initial pose (random-pose timeline detected)"
            )
        if ground_animate:
            warnings.append(
                "Ping-pong animation for "
                + ", ".join(ground_animate)
                + " layer(s) over their authored pose span (random-pose timeline detected)"
            )
        precomputed = prepared.get("precomputed_loop") or {}
        if precomputed.get("frame_count"):
            frame_count = int(precomputed["frame_count"])
            detected_item_loop = precomputed.get("detected_item_loop")
            detected_blink_frames = precomputed.get("detected_blink_frames")
            precomputed_static = tuple(precomputed.get("static_keys") or ())
            static_keys = tuple(dict.fromkeys(static_keys + precomputed_static))
            precomputed_animate = precomputed.get("ground_animate") or {}
            ground_animate = {
                key: int(span) for key, span in precomputed_animate.items() if int(span) >= 2
            }
            detected_loop = (
                character_svg.aligned_animation_frame_count(
                    detected_item_loop, detected_blink_frames
                )
                if detected_item_loop is not None and detected_blink_frames is not None
                else None
            )
            ignored_loop_keys = ("armor_head",)
        elif request.render.complete_loop:
            loop_sigs, ignored_loop_keys = character_svg.loop_driver_from_signatures(detection_sigs)
            loop_sigs = {key: sigs for key, sigs in loop_sigs.items() if key not in static_keys}
            detected_item_loop = character_svg.detect_loop_from_signatures(
                loop_sigs, max_frames=request.render.max_frames
            )
            detected_blink_frames = (
                character_svg.detect_loop_from_signatures(
                    {"armor_head": detection_sigs["armor_head"]},
                    max_frames=request.render.max_frames,
                )
                if "armor_head" in detection_sigs
                else None
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
        component_mode = bool(config.component_raster_enabled)
        if component_mode:
            # Component-raster safety cap (docs/component-raster-pipeline.md):
            # rasterize each unique placed component state once, then compose
            # at most this many output frames across parallel frame chunks.
            capped_frame_count = min(frame_count, config.component_raster_frame_cap)
            if capped_frame_count < frame_count:
                warnings.append(
                    f"The component-raster experiment caps this job at "
                    f"{config.component_raster_frame_cap} output frame(s)"
                )
            frame_count = capped_frame_count
        mark("component_cap_ms", phase)

        phase = time.perf_counter()
        layers = character_svg.build_layers(
            prepared["aliases"], weapon_type=prepared["weapon_type"]
        )
        viewbox = shared_viewbox_from_bounds(
            layers,
            symbol_bounds,
            frame_count=frame_count,
            facing=request.render.facing,
            zoom=float(prepared["settings"]["zoom"]),
            max_size=int(prepared["settings"]["output_size"]),
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
        frame_durations = character_svg.frame_durations_for_rate(frame_count, frame_rate)
        batches = [
            batch.to_dict()
            for batch in partition_frames(frame_count, config.frames_per_render_lambda)
        ]
        component_batches = [
            batch.to_dict()
            for batch in partition_frames(
                frame_count,
                config.component_compose_frames_per_lambda,
            )
        ]
        component_tasks: list[dict[str, Any]] = []
        component_frames: list[dict[str, Any]] = []
        if component_mode:
            phase = time.perf_counter()
            component_tasks, component_frames = build_component_manifest(
                job_id=request.job_id,
                layers=layers,
                symbol_signatures=symbol_signatures,
                part_manifest=part_manifest,
                frame_count=frame_count,
                frame_durations=frame_durations,
                facing=request.render.facing,
                weapon_type=prepared["weapon_type"],
                viewbox=viewbox,
                raster_size=int(prepared["settings"]["raster_size"]),
                output_size=int(prepared["settings"]["output_size"]),
                fields=prepared["fields"],
                static_keys=static_keys,
                ground_animate=ground_animate,
                detected_blink_frames=detected_blink_frames,
                ignored_loop_keys=ignored_loop_keys,
                source_bundle_frame_count=config.source_bundle_frame_count,
                renderer_version=config.renderer_version,
            )
            mark("component_manifest_ms", phase)
        component_raster_space: str | None = None
        if component_mode:
            raster_size = int(prepared["settings"]["raster_size"])
            output_size = int(prepared["settings"]["output_size"])
            # Pillow's reducing-gap prepass changes sampling behavior above a
            # 2x shrink. Keep the legacy full-frame path for uncommon larger
            # ratios; every exposed Discord output preset currently uses 2x.
            component_raster_space = "output" if raster_size <= output_size * 2 else "raster"
        manifest_key = f"jobs/{request.job_id}/prepare/manifest.json"
        manifest = {
            "schema_version": 1,
            "job_id": request.job_id,
            "render_hash": prepared["render_hash"],
            "final_key": prepared["final_key"],
            "frame_count": frame_count,
            "frame_rate": frame_rate,
            "viewbox": list(viewbox),
            "frame_durations": frame_durations,
            "fields": dict(sorted(prepared["fields"].items())),
            "aliases": dict(sorted(prepared["aliases"].items())),
            "weapon_type": prepared["weapon_type"],
            "parts": part_manifest,
            "static_keys": list(static_keys),
            "ground_animate": {key: span for key, span in sorted(ground_animate.items())},
            # Reconstruct the per-source bundle keys deterministically instead
            # of shipping ~5000 entries through Step Functions state.
            "source_bundles": {
                str(int(result["source_idx"])): {
                    str(ordinal): (
                        f"jobs/{request.job_id}/prepare/source-bundles/"
                        f"{int(result['source_idx'])}.{ordinal}.tar.gz"
                    )
                    for ordinal in range(
                        (int(prepared["export_frame_count"]) + config.source_bundle_frame_count - 1)
                        // config.source_bundle_frame_count
                    )
                }
                for result in export_key_to_result.values()
            },
            "all_color_rules": sorted(all_color_rules),
            "settings": request.render.to_dict(),
            "batches": batches,
            "component_batches": component_batches,
            "component_pipeline": component_mode,
            # New jobs downsample each already-rasterized 2x component onto
            # the final output pixel grid before the compose Map. Compositors
            # retain a raster-grid fallback for manifests created by an older
            # deployment during a rolling update.
            "component_raster_space": component_raster_space,
            "component_tasks": component_tasks,
            "component_frames": component_frames,
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
            symbol_loops = {
                key: {
                    "period": character_svg.detect_loop_from_signatures(
                        {key: sigs},
                        max_frames=request.render.max_frames,
                        validation_frames=character_svg.LOOP_VALIDATION_FRAMES,
                    ),
                    "unique_states": len(set(sigs[: request.render.max_frames])),
                }
                for key, sigs in sorted(detection_sigs.items())
            }
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
            "component_batches": component_batches,
            "component_pipeline": component_mode,
            # The full task records live in the S3 manifest. The workflow Map
            # only needs compact indexes because each component Lambda already
            # reads that manifest before rasterizing its selected task.
            "component_task_indices": list(range(len(component_tasks))),
            "frame_count": frame_count,
        }
