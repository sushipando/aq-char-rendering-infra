#!/usr/bin/env python3
"""Compose a complete AQW character as SVG without Flash/AIR.

The official ``characterB.swf`` does not contain a single, exportable character
symbol. At runtime it loads an armor library, inserts eleven separately exported
body-part classes into fixed holders, then interleaves the cape, weapons, helm,
and ground item in the display list. This script reproduces that assembly using
FFDec SVG exports and the exact matrices from ``characterB.swf``.

Unlike :mod:`pipeline.preview_aqw_tryon`, this renderer never executes AIR. It
also handles AQW's common ``mcSetColor(this, location, shade)`` frame scripts by
translating them into SVG color-matrix filters. Other ActionScript-driven
animation or runtime effects remain static at the selected Ready/Idle frame.
With ``--frames`` or ``--complete-loop``, FFDec advances nested item timelines
while that character pose remains fixed, producing aligned SVG/PNG frames and
an optional WebP.

Example using a locally saved character response::

    ./venv/bin/python pipeline/render_swf_character_svg.py Tdnq \
      --flashvars-json bot/assets/tdnq_render/tdnq_flashvars.json \
      --base-items \
      --output render_outputs/character_svg/tdnq.svg \
      --preview-png render_outputs/character_svg/tdnq.png

One item can be overridden in the same way as the native try-on proof of
concept::

    ./venv/bin/python pipeline/render_swf_character_svg.py Tdnq \
      --item-id 12345 \
      --output render_outputs/character_svg/tdnq_tryon.svg
"""

from __future__ import annotations

import argparse
from collections import defaultdict
from concurrent.futures import ThreadPoolExecutor
import copy
from dataclasses import dataclass
import hashlib
import json
import math
import os
from pathlib import Path, PurePosixPath
import re
import shutil
import subprocess
import sys
import tempfile
from time import perf_counter
from typing import Iterable, Mapping, Sequence
from urllib.parse import quote
import xml.etree.ElementTree as ET

from PIL import Image, ImageChops

from aqw_char_renderer.legacy import preview_aqw_tryon as tryon
from aqw_char_renderer.legacy import render_swf_items as item_renderer


REPO_ROOT = Path(__file__).resolve().parents[4]
DEFAULT_OUTPUT_DIR = REPO_ROOT / "render_outputs" / "character_svg"
DEFAULT_ASSET_CACHE = REPO_ROOT / "tmp" / "character_svg_assets"
DEFAULT_FFDEC = Path("/Applications/FFDec.app/Contents/Resources/ffdec-cli.jar")
DEFAULT_CHARACTER_RENDERER = Path.home() / "Projects" / "aq" / "swf" / "characterB.swf"

SVG_NS = item_renderer.SVG_NAMESPACE
XLINK_NS = item_renderer.XLINK_NAMESPACE
FFDEC_NS = item_renderer.FFDEC_NAMESPACE
ET.register_namespace("", SVG_NS)
ET.register_namespace("xlink", XLINK_NS)
ET.register_namespace("ffdec", FFDEC_NS)

Matrix = tuple[float, float, float, float, float, float]
IDENTITY: Matrix = (1.0, 0.0, 0.0, 1.0, 0.0, 0.0)
CHARACTER_DISPLAY_SCALE = item_renderer.CHARACTER_DETAIL_DISPLAY_SCALE
DEFAULT_LOOP_MAX_FRAMES = 120
LOOP_VALIDATION_FRAMES = 8

# These are the complete character-space transforms, including mcChar. They
# come from DefineSprite 126 in the official characterB.swf. Translations in
# the SWF XML are twips and have already been divided by 20.
PART_TRANSFORMS: dict[str, Matrix] = {
    "weapon": (0.059173885, -0.208672928, -0.208758253, -0.059147657, -10.760687280, -47.265744330),
    "weapon_off": (0.038950285, -0.211466225, -0.211645155, -0.038892447, 25.176562560, -50.366785000),
    "helm": (0.237343166, 0.0, 0.0, 0.237079071, 5.656329360, -100.733687495),
    "cape": (0.287392009, 0.006609274, 0.031190777, 0.342461126, -9.209079000, -93.481253670),
    "head": (0.237245421, 0.0, 0.0, 0.237079071, 5.656329, -100.733687),
    "chest": (0.266389399, 0.042586311, -0.042616193, 0.266202614, 1.752283, -76.475547),
    "hip": (0.270101136, 0.009417834, -0.009424442, 0.269911749, 2.653217, -60.220092),
    "front_shoulder": (0.213386460, 0.161263218, -0.161376371, 0.213236840, -10.660584, -81.077091),
    "back_shoulder": (-0.235259722, 0.053927397, 0.053965236, 0.235094765, 8.659442, -78.076084),
    "front_hand": (-0.258248030, 0.054080039, -0.054117985, -0.258082209, -17.467639, -61.770612),
    "back_hand": (-0.196996792, 0.135406161, -0.135470619, -0.196873928, 16.267328, -57.569202),
    "front_thigh": (0.260783609, 0.061208285, -0.061251232, 0.260600754, -5.204929, -49.466483),
    "back_thigh": (0.255132001, -0.123332405, 0.066780646, 0.328006195, 11.212088, -50.566852),
    "front_shin": (0.265595116, 0.040739380, -0.041531696, 0.270384938, -15.165253, -30.910256),
    "back_shin": (0.255773536, -0.085126837, 0.086790400, 0.260356542, 10.661517, -28.159332),
    "idle_foot": (0.206314310, 0.022392159, -0.022407870, 0.206169648, -24.875317, -5.501729),
    "back_foot": (0.239781009, 0.003709130, -0.003711733, 0.239612881, 16.918002, -7.152283),
    "robe": (1.000426617, 0.034908565, -0.034933059, 0.999725145, 1.752283, -61.870646),
    "back_robe": (1.000502986, -0.008715694, 0.008721809, 0.999801461, -3.953632, -66.922341),
    "gauntlet_front": (0.206598424, -0.043264031, -0.043294388, -0.206465767, -17.467639200, -61.770611980),
    "gauntlet_back": (0.157597434, -0.108324928, -0.108376495, -0.157499143, 16.267327920, -57.569202040),
    "ground": item_renderer.CHARACTER_GROUND_TRANSFORM,
}

_BACKHAIR_HOLDER: Matrix = (
    0.89178467,
    0.10942078,
    -0.10942078,
    0.89178467,
    80 / 20,
    -2283 / 20,
)
PART_TRANSFORMS["backhair"] = item_renderer.compose_transforms(
    item_renderer.CHARACTER_MC_TRANSFORM,
    _BACKHAIR_HOLDER,
)

ARMOR_PART_CLASSES = {
    "head": "Head",
    "chest": "Chest",
    "hip": "Hip",
    "idle_foot": "FootIdle",
    "back_foot": "Foot",
    "shoulder": "Shoulder",
    "hand": "Hand",
    "thigh": "Thigh",
    "shin": "Shin",
    "robe": "Robe",
    "back_robe": "RobeBack",
}
REQUIRED_ARMOR_PARTS = frozenset(
    {"chest", "hip", "idle_foot", "back_foot", "shoulder", "hand", "thigh", "shin"}
)
SUPPORTED_OVERRIDE_SLOTS = frozenset({"armor", "weapon", "helm", "cape", "ground"})

_MATRIX_RE = re.compile(
    r"matrix\(\s*([-+0-9.eE]+)[,\s]+([-+0-9.eE]+)[,\s]+"
    r"([-+0-9.eE]+)[,\s]+([-+0-9.eE]+)[,\s]+"
    r"([-+0-9.eE]+)[,\s]+([-+0-9.eE]+)\s*\)"
)
_COLOR_CALL_RE = re.compile(
    r"(?:mcSetColor|setColor)\s*\(\s*this\s*,\s*['\"]([^'\"]+)['\"]"
    r"\s*,\s*['\"]([^'\"]+)['\"]\s*\)"
)
_PACKAGE_RE = re.compile(r"\bpackage(?:\s+([A-Za-z_][\w.]*))?\s*\{")
_CLASS_RE = re.compile(r"\bclass\s+([A-Za-z_]\w*)\b")
_URL_REF_RE = re.compile(r"url\(#([^)]+)\)")

_FFDEC_SMALL_STROKE = f"{{{FFDEC_NS}}}has-small-stroke"
_FFDEC_ORIGINAL_STROKE_WIDTH = f"{{{FFDEC_NS}}}original-stroke-width"
_COMPENSATED_STROKE_WIDTH = "data-aqw-ffdec-compensated-stroke-width"
_AUTHORED_STROKE_WIDTH = "data-aqw-authored-stroke-width"
_SYMBOL_MINIMUM_STROKE_SCALE = "data-aqw-symbol-minimum-stroke-scale"
_LAYER_STROKE_SCALE = "data-aqw-layer-stroke-scale"


class CharacterSvgError(RuntimeError):
    """Raised when a character cannot be reconstructed as static SVG."""


@dataclass(frozen=True)
class AppearanceAsset:
    slot: str
    remote_path: str
    link: str
    weapon_type: str = ""


@dataclass(frozen=True)
class SymbolRequest:
    key: str
    source: Path
    class_name: str
    character_id: int
    frame: int


@dataclass
class ImportedSymbol:
    key: str
    definition: ET.Element
    definitions: list[ET.Element]
    bounds: tuple[float, float, float, float]
    export_zoom: float = 1.0
    # Apparent width, in this imported symbol's coordinates, of a stroke that
    # FFDec expanded to Flash's one-pixel minimum. Raw FFDec symbols lose their
    # outer zoom wrapper, while retained character-space SVGs keep additional
    # holder transforms around that wrapper.
    minimum_stroke_scale: float = 1.0


@dataclass(frozen=True)
class Layer:
    name: str
    symbol_key: str
    transform: Matrix
    darken: bool = False


def matrix_text(matrix: Matrix) -> str:
    return "matrix(" + " ".join(f"{value:.12g}" for value in matrix) + ")"


def invert_transform(matrix: Matrix) -> Matrix:
    """Return the inverse of one SVG affine transform."""
    a, b, c, d, e, f = matrix
    determinant = a * d - b * c
    if abs(determinant) < 1e-12:
        raise CharacterSvgError(f"Cannot invert singular SVG transform: {matrix}")
    inverse = (
        d / determinant,
        -b / determinant,
        -c / determinant,
        a / determinant,
        0.0,
        0.0,
    )
    ia, ib, ic, id_, _, _ = inverse
    return (
        ia,
        ib,
        ic,
        id_,
        -(ia * e + ic * f),
        -(ib * e + id_ * f),
    )


def parse_matrix(value: str | None) -> Matrix | None:
    if not value:
        return None
    match = _MATRIX_RE.fullmatch(value.strip())
    if match is None:
        return None
    values = tuple(float(value) for value in match.groups())
    if not all(math.isfinite(value) for value in values):
        return None
    return values  # type: ignore[return-value]


def swf_frame_rate(path: Path) -> float:
    """Read the SWF header's 8.8 fixed-point frame rate."""
    try:
        data = tryon.decompressed_swf(path)
    except (OSError, tryon.TryOnError) as error:
        raise CharacterSvgError(f"Cannot read frame rate from {path}: {error}") from error
    if len(data) < 12:
        raise CharacterSvgError(f"SWF header is truncated: {path}")
    rect_bits = 5 + 4 * (data[8] >> 3)
    frame_rate_offset = 8 + (rect_bits + 7) // 8
    if frame_rate_offset + 2 > len(data):
        raise CharacterSvgError(f"SWF frame-rate field is truncated: {path}")
    rate = int.from_bytes(
        data[frame_rate_offset : frame_rate_offset + 2],
        "little",
    ) / 256
    if not math.isfinite(rate) or rate <= 0:
        raise CharacterSvgError(f"SWF has an invalid frame rate {rate}: {path}")
    return rate


def frame_durations_for_rate(frame_count: int, frame_rate: float) -> list[int]:
    """Distribute integer WebP milliseconds at the requested frame rate."""
    if frame_count < 1:
        raise CharacterSvgError("Frame count must be positive")
    if not math.isfinite(frame_rate) or frame_rate <= 0 or frame_rate > 1000:
        raise CharacterSvgError("Frame rate must be between 0 and 1000 FPS")
    timestamps = [round(index * 1000 / frame_rate) for index in range(frame_count + 1)]
    durations = [end - start for start, end in zip(timestamps, timestamps[1:])]
    if any(duration < 1 for duration in durations):
        raise CharacterSvgError(f"Frame rate {frame_rate:g} cannot be encoded in milliseconds")
    return durations


def svg_slot_transform(slot: str, weapon_type: str = "Sword") -> Matrix:
    """Return the transform already present in a game-space item SVG."""
    if slot == "weapon":
        return (
            PART_TRANSFORMS["gauntlet_front"]
            if weapon_type.casefold() == "gauntlet"
            else PART_TRANSFORMS["weapon"]
        )
    try:
        return PART_TRANSFORMS[slot]
    except KeyError as error:
        raise CharacterSvgError(
            f"SVG overrides are not supported for the {slot!r} slot"
        ) from error


def _visibility_flags(fields: Mapping[str, str]) -> int:
    try:
        return int(fields.get("ia1", "0") or 0)
    except (TypeError, ValueError):
        return 0


def _chosen_fields(
    fields: Mapping[str, str],
    title: str,
    base_file: str,
    base_link: str,
    *,
    use_cosmetics: bool,
) -> tuple[str, str]:
    if use_cosmetics and f"strCust{title}Name" in fields:
        return fields.get(f"strCust{title}File", ""), fields.get(
            f"strCust{title}Link", ""
        )
    return fields.get(base_file, ""), fields.get(base_link, "")


def appearance_assets(
    fields: Mapping[str, str],
    *,
    use_cosmetics: bool = True,
) -> dict[str, AppearanceAsset]:
    """Resolve the visible characterB slots from one FlashVars mapping."""
    gender = str(fields.get("strGender", "M") or "M").upper()
    if gender not in {"M", "F"}:
        raise CharacterSvgError(f"Unsupported character gender: {gender!r}")
    flags = _visibility_flags(fields)
    assets: dict[str, AppearanceAsset] = {}

    armor_file, armor_link = _chosen_fields(
        fields,
        "Armor",
        "strClassFile",
        "strClassLink",
        use_cosmetics=use_cosmetics,
    )
    if armor_file and armor_file.casefold() != "none":
        normalized_armor_file = armor_file.replace("\\", "/").lstrip("/")
        armor_path = tryon.normalize_asset_path(
            normalized_armor_file
            if normalized_armor_file.casefold().startswith("classes/")
            else f"classes/{gender}/{normalized_armor_file}"
        )
        assets["armor"] = AppearanceAsset("armor", armor_path, armor_link)

    weapon_file, weapon_link = _chosen_fields(
        fields,
        "Weapon",
        "strWeaponFile",
        "strWeaponLink",
        use_cosmetics=use_cosmetics,
    )
    if weapon_file and weapon_file.casefold() != "none":
        type_field = (
            "strCustWeaponType"
            if use_cosmetics and "strCustWeaponName" in fields
            else "strWeaponType"
        )
        assets["weapon"] = AppearanceAsset(
            "weapon",
            tryon.normalize_asset_path(weapon_file),
            weapon_link,
            str(fields.get(type_field, "Sword") or "Sword"),
        )

    helm_file, helm_link = _chosen_fields(
        fields,
        "Helm",
        "strHelmFile",
        "strHelmLink",
        use_cosmetics=use_cosmetics,
    )
    if not flags & (1 << 1) and helm_file and helm_file.casefold() != "none":
        assets["helm"] = AppearanceAsset(
            "helm", tryon.normalize_asset_path(helm_file), helm_link
        )
    else:
        hair_file = str(fields.get("strHairFile", "") or "")
        hair_name = str(fields.get("strHairName", "") or "")
        if (
            hair_file
            and hair_file.casefold() != "none"
            and hair_name
            and hair_name.casefold() != "blank"
        ):
            assets["hair"] = AppearanceAsset(
                "hair",
                tryon.normalize_asset_path(hair_file),
                f"{hair_name}{gender}Hair",
            )

    cape_file, cape_link = _chosen_fields(
        fields,
        "Cape",
        "strCapeFile",
        "strCapeLink",
        use_cosmetics=use_cosmetics,
    )
    if not flags & 1 and cape_file and cape_file.casefold() != "none":
        assets["cape"] = AppearanceAsset(
            "cape", tryon.normalize_asset_path(cape_file), cape_link
        )

    ground_file = str(fields.get("strMiscFile", "") or "")
    ground_link = str(fields.get("strMiscLink", "") or "")
    if ground_file and ground_file.casefold() != "none":
        assets["ground"] = AppearanceAsset(
            "ground", tryon.normalize_asset_path(ground_file), ground_link
        )
    return assets


def load_flashvars(path: Path) -> dict[str, str]:
    try:
        payload = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise CharacterSvgError(f"Unable to read FlashVars JSON {path}: {error}") from error
    if isinstance(payload, dict) and isinstance(payload.get("flashvars"), dict):
        payload = payload["flashvars"]
    if not isinstance(payload, dict):
        raise CharacterSvgError(f"Expected an object or {{'flashvars': object}} in {path}")
    fields = {str(key): str(value) for key, value in payload.items()}
    if "strGender" not in fields:
        raise CharacterSvgError(f"FlashVars JSON has no strGender: {path}")
    return fields


def _apply_override(
    fields: dict[str, str],
    *,
    slot: str,
    source: Path,
    link: str,
    name: str,
    weapon_type: str | None,
    custom: bool = True,
) -> str:
    """Apply an SVG renderer override and return its synthetic remote path."""
    filename = source.name
    gender = fields.get("strGender", "M").upper()
    if slot == "armor":
        prefix = "strCustArmor" if custom else "strClass"
        fields[f"{prefix}File"] = filename
        fields[f"{prefix}Link"] = link
        fields["strCustArmorName" if custom else "strArmorName"] = name
        return f"classes/{gender}/{filename}"
    if slot == "ground":
        path = f"overrides/ground/{filename}"
        fields.update(
            {"strMiscFile": path, "strMiscLink": link, "strMiscName": name}
        )
        return path

    title = slot.capitalize()
    path = f"overrides/{slot}/{filename}"
    prefix = f"strCust{title}" if custom else f"str{title}"
    fields.update({f"{prefix}File": path, f"{prefix}Link": link, f"{prefix}Name": name})
    if slot == "weapon":
        fields["strCustWeaponType" if custom else "strWeaponType"] = weapon_type or "Sword"
    if slot in {"cape", "helm"}:
        bit = 0 if slot == "cape" else 1
        fields["ia1"] = str(_visibility_flags(fields) & ~(1 << bit))
    return path


def _resolve_override(
    args: argparse.Namespace,
    fields: dict[str, str],
    resolver: tryon.LocalAssetResolver,
) -> tuple[str, Path, str, str, str | None] | None:
    if args.item_id is None and args.swf is None:
        return None
    database_slot = ""
    if args.item_id is not None:
        record = tryon.load_item(args.database, args.item_id)
        database_slot = str(record.get("slot") or "")
        slot = tryon.effective_slot(database_slot)
        if slot not in SUPPORTED_OVERRIDE_SLOTS:
            raise CharacterSvgError(
                f"Item {args.item_id} has unsupported on-character slot {database_slot!r}"
            )
        if args.slot is not None and args.slot != slot:
            raise CharacterSvgError(
                f"--slot {args.slot} conflicts with item {args.item_id}'s {slot} slot"
            )
        name = args.name or str(record.get("name") or f"Item {args.item_id}")
        raw_file = str(record.get("file") or "")
        remote = (
            f"classes/{fields.get('strGender', 'M').upper()}/{raw_file}"
            if slot == "armor" and "/" not in raw_file
            else tryon.normalize_asset_path(raw_file)
        )
        source = resolver.resolve(remote)
        if source is None:
            raise CharacterSvgError(
                f"Item {args.item_id}'s SWF is not in the local archive: {remote}"
            )
    else:
        if args.slot is None:
            raise CharacterSvgError("--slot is required with --swf")
        slot = args.slot
        source = args.swf.expanduser().resolve()
        name = args.name or source.stem
        if not tryon.has_valid_swf_header(source):
            raise CharacterSvgError(f"Override is not a valid SWF: {source}")
    link = args.link or tryon.infer_export_link(
        source, slot=slot, gender=fields.get("strGender", "M")
    )
    weapon_type = None
    if slot == "weapon":
        weapon_type = args.weapon_type or tryon.infer_weapon_type(source, database_slot)
    return slot, source, link, name, weapon_type


def resolve_asset_source(
    asset: AppearanceAsset,
    *,
    resolver: tryon.LocalAssetResolver,
    explicit: Mapping[str, Path],
    cache_dir: Path,
    timeout: float,
    offline: bool,
) -> Path:
    key = asset.remote_path.casefold()
    source = explicit.get(key) or resolver.resolve(asset.remote_path)
    if source is not None:
        return source
    cached = cache_dir.joinpath(*PurePosixPath(asset.remote_path).parts)
    if tryon.has_valid_swf_header(cached):
        return cached
    if offline:
        raise CharacterSvgError(f"Missing local SWF in offline mode: {asset.remote_path}")
    url = f"{tryon.GAMEFILES_URL}{quote(asset.remote_path, safe='/')}"
    print(f"Downloading missing character asset: {asset.remote_path}")
    tryon.download_swf(url, cached, timeout=timeout)
    return cached


def symbol_id(source: Path, class_name: str, *, required: bool = True) -> tuple[int, str] | None:
    entries = tryon.symbol_class_entries(source)
    wanted = class_name.casefold()
    match = next(
        (
            (character_id, actual_name)
            for character_id, actual_name in entries
            if actual_name.casefold() == wanted
        ),
        None,
    )
    # Very old AQW assets can intentionally provide an empty sLink and contain
    # no SymbolClass table. The game loads their compiled root directly. Match
    # the static renderer's deterministic rule: the highest DefineSprite id is
    # the composed root rather than one of its lower-id dependencies.
    if match is None and not class_name:
        if len(entries) == 1:
            match = entries[0]
        else:
            try:
                metadata = item_renderer.parse_swf_sprite_metadata(
                    source.read_bytes(), source.stem
                )
            except OSError:
                metadata = {}
            root_id = metadata.get("root")
            if isinstance(root_id, int) and root_id > 0:
                root_name = next(
                    (name for character_id, name in entries if character_id == root_id),
                    source.stem,
                )
                match = (root_id, root_name)
    if match is None and required:
        raise CharacterSvgError(f"{source.name} does not export class {class_name!r}")
    return match


def _symbol_frame(source: Path, class_name: str) -> int:
    try:
        metadata = item_renderer.parse_swf_sprite_metadata(source.read_bytes(), class_name)
    except OSError:
        metadata = {}
    return item_renderer.select_static_root_frame(metadata)


def build_symbol_requests(
    assets: Mapping[str, AppearanceAsset],
    sources: Mapping[str, Path],
    *,
    character_renderer: Path,
    gender: str,
) -> tuple[list[SymbolRequest], dict[str, str], list[str]]:
    """Return unique symbols, semantic key aliases, and nonfatal warnings."""
    requests: list[SymbolRequest] = []
    aliases: dict[str, str] = {}
    warnings: list[str] = []

    armor = assets.get("armor")
    if armor is None:
        raise CharacterSvgError("The character has no visible armor SWF")
    armor_source = sources["armor"]
    armor_root = armor.link
    for logical, suffix in ARMOR_PART_CLASSES.items():
        class_name = f"{armor_root}{gender}{suffix}"
        found = symbol_id(
            armor_source,
            class_name,
            required=logical in REQUIRED_ARMOR_PARTS,
        )
        if found is None:
            if logical == "head":
                fallback_name = f"mcHead{gender}"
                fallback = symbol_id(character_renderer, fallback_name)
                assert fallback is not None
                key = "armor_head"
                requests.append(
                    SymbolRequest(
                        key,
                        character_renderer,
                        fallback[1],
                        fallback[0],
                        _symbol_frame(character_renderer, fallback[1]),
                    )
                )
                aliases[logical] = key
                warnings.append(
                    f"{armor_source.name} has no {class_name}; used {fallback_name}"
                )
            continue
        key = f"armor_{logical}"
        requests.append(
            SymbolRequest(
                key,
                armor_source,
                found[1],
                found[0],
                _symbol_frame(armor_source, found[1]),
            )
        )
        aliases[logical] = key

    for slot in ("weapon", "cape", "helm", "ground", "hair"):
        asset = assets.get(slot)
        if asset is None:
            continue
        source = sources[slot]
        found = symbol_id(source, asset.link)
        assert found is not None
        key = slot
        requests.append(
            SymbolRequest(
                key,
                source,
                found[1],
                found[0],
                _symbol_frame(source, found[1]),
            )
        )
        aliases[slot] = key

        if slot == "helm":
            backhair = symbol_id(source, f"{asset.link}_backhair", required=False)
            if backhair is not None:
                requests.append(
                    SymbolRequest(
                        "backhair",
                        source,
                        backhair[1],
                        backhair[0],
                        _symbol_frame(source, backhair[1]),
                    )
                )
                aliases["backhair"] = "backhair"
        elif slot == "hair":
            back_name = re.sub(r"Hair$", "HairBack", asset.link)
            backhair = symbol_id(source, back_name, required=False)
            if backhair is not None:
                requests.append(
                    SymbolRequest(
                        "backhair",
                        source,
                        backhair[1],
                        backhair[0],
                        _symbol_frame(source, backhair[1]),
                    )
                )
                aliases["backhair"] = "backhair"

    # The same exported class can satisfy multiple semantic requests. FFDec only
    # needs to export it once; aliases still point at the first definition.
    unique: dict[tuple[Path, int, int], SymbolRequest] = {}
    remapped: dict[str, str] = {}
    for request in requests:
        signature = (request.source, request.character_id, request.frame)
        previous = unique.get(signature)
        if previous is None:
            unique[signature] = request
            remapped[request.key] = request.key
        else:
            remapped[request.key] = previous.key
    aliases = {name: remapped.get(key, key) for name, key in aliases.items()}
    return list(unique.values()), aliases, warnings


def _ffdec_command(ffdec: Path, *arguments: str, home: Path) -> list[str]:
    home.mkdir(parents=True, exist_ok=True)
    return [
        "java",
        f"-Duser.home={home}",
        "-Djava.awt.headless=true",
        "-jar",
        str(ffdec),
        *arguments,
    ]


def export_requested_symbol_frames(
    requests: Sequence[SymbolRequest],
    *,
    ffdec: Path,
    zoom: float,
    destination: Path,
    subframe_start: int = 1,
    frame_count: int = 1,
) -> dict[str, list[Path]]:
    """Export one or more nested timeline states for every requested symbol."""
    if subframe_start < 1 or frame_count < 1:
        raise CharacterSvgError("Subframe start and frame count must be positive")
    grouped: dict[Path, list[SymbolRequest]] = defaultdict(list)
    for request in requests:
        grouped[request.source].append(request)
    exported: dict[str, list[Path]] = {}
    ffdec_home = destination / ".ffdec-home"
    subframe_end = subframe_start + frame_count - 1

    for index, (source, group) in enumerate(grouped.items()):
        output = destination / f"asset_{index:02d}"
        selected_ids = ",".join(str(request.character_id) for request in group)
        selected_frames = ",".join(
            f"{request.character_id}:{request.frame}" for request in group
        )
        arguments: list[str] = []
        if zoom != 1:
            arguments.extend(("-zoom", f"{zoom:.12g}"))
        if frame_count > 1 or subframe_start > 1:
            arguments.extend(("-sublength", str(subframe_end)))
        arguments.extend(
            (
                "-selectid",
                selected_ids,
                "-select",
                selected_frames,
                "-format",
                "sprite:svg",
                "-export",
                "sprite",
                str(output),
                str(source),
            )
        )
        command = _ffdec_command(ffdec, *arguments, home=ffdec_home)
        result = subprocess.run(command, capture_output=True, text=True)
        if result.returncode:
            detail = (result.stderr or result.stdout).strip()
            raise CharacterSvgError(
                f"FFDec SVG export failed for {source}: {detail[-2000:]}"
            )
        for request in group:
            generic_directory = output / f"DefineSprite_{request.character_id}"
            directories = sorted(
                {
                    *output.glob(f"DefineSprite_{request.character_id}_*"),
                    *([generic_directory] if generic_directory.is_dir() else []),
                }
            )
            frames: list[Path] = []
            for subframe in range(subframe_start, subframe_end + 1):
                candidates = [
                    directory / str(request.frame) / f"{subframe}.svg"
                    for directory in directories
                ]
                if frame_count == 1 and subframe_start == 1:
                    candidates.extend(
                        directory / f"{request.frame}.svg" for directory in directories
                    )
                frame = next((candidate for candidate in candidates if candidate.is_file()), None)
                if frame is None:
                    raise CharacterSvgError(
                        f"FFDec did not export sprite {request.character_id} root frame "
                        f"{request.frame}, nested frame {subframe} "
                        f"({request.class_name}) from {source.name}"
                    )
                frames.append(frame)
            exported[request.key] = frames
    return exported


def export_requested_symbols(
    requests: Sequence[SymbolRequest],
    *,
    ffdec: Path,
    zoom: float,
    destination: Path,
) -> dict[str, Path]:
    """Backward-compatible single-frame wrapper."""
    frames = export_requested_symbol_frames(
        requests,
        ffdec=ffdec,
        zoom=zoom,
        destination=destination,
    )
    return {key: paths[0] for key, paths in frames.items()}


def combined_frame_signatures(
    exports: Mapping[str, Sequence[Path]],
) -> list[bytes]:
    """Hash the complete FFDec state for each exported nested frame."""
    if not exports:
        return []
    frame_count = min(len(paths) for paths in exports.values())
    signatures: list[bytes] = []
    ordered_keys = sorted(exports)
    for frame_index in range(frame_count):
        combined = hashlib.sha256()
        for key in ordered_keys:
            combined.update(key.encode("utf-8"))
            combined.update(b"\0")
            try:
                payload = exports[key][frame_index].read_bytes()
            except OSError as error:
                raise CharacterSvgError(
                    f"Cannot inspect FFDec loop frame {exports[key][frame_index]}: {error}"
                ) from error
            combined.update(hashlib.sha256(payload).digest())
        signatures.append(combined.digest())
    return signatures


def frame_state_pattern(paths: Sequence[Path]) -> tuple[int, ...]:
    """Represent a timeline by equality between its exported SVG states."""
    states: dict[bytes, int] = {}
    pattern: list[int] = []
    for path in paths:
        try:
            signature = hashlib.sha256(path.read_bytes()).digest()
        except OSError as error:
            raise CharacterSvgError(
                f"Cannot inspect FFDec loop frame {path}: {error}"
            ) from error
        pattern.append(states.setdefault(signature, len(states)))
    return tuple(pattern)


def loop_driver_exports(
    exports: Mapping[str, Sequence[Path]],
) -> tuple[dict[str, Sequence[Path]], tuple[str, ...]]:
    """Exclude the character blink while retaining independent item motion.

    AQW armor heads commonly contain the natural eye-blink timeline. A helm,
    hair, or back-hair symbol can contain a matching face layer. Such a symbol
    is ignored only when its complete state-change schedule exactly matches
    the armor head; independently animated head equipment remains a loop
    driver.
    """
    drivers = dict(exports)
    head_paths = drivers.pop("armor_head", None)
    if head_paths is None:
        return drivers, ()
    ignored = ["armor_head"]
    head_pattern = frame_state_pattern(head_paths)
    for key in ("helm", "hair", "backhair"):
        paths = drivers.get(key)
        if paths is not None and frame_state_pattern(paths) == head_pattern:
            drivers.pop(key)
            ignored.append(key)
    return drivers, tuple(ignored)


def detect_complete_loop_frame_count(
    exports: Mapping[str, Sequence[Path]],
    *,
    max_frames: int,
    validation_frames: int = LOOP_VALIDATION_FRAMES,
) -> int | None:
    """Return the full combined loop from each exported symbol's period.

    The exporter scans a few states beyond the output cap. Those trailing
    states validate a symbol period whose final unique frame is at the cap
    instead of mistaking one coincidentally repeated pose for a complete
    cycle. The complete character loop is the least common multiple of those
    periods and can therefore be larger than ``max_frames``.
    """
    if max_frames < 1 or validation_frames < 1:
        raise CharacterSvgError("Loop frame limits must be positive")
    periods: list[int] = []
    for key, paths in exports.items():
        signatures = combined_frame_signatures({key: paths})
        search_limit = min(max_frames, len(signatures) - 1)
        symbol_period = next(
            (
                period
                for period in range(1, search_limit + 1)
                if len(signatures) - period >= min(validation_frames, period)
                and all(
                    signatures[index] == signatures[index % period]
                    for index in range(period, len(signatures))
                )
            ),
            None,
        )
        if symbol_period is None:
            return None
        periods.append(symbol_period)
    return math.lcm(*periods) if periods else 1


def parse_color_scripts(
    source: Path,
    *,
    ffdec: Path,
    destination: Path,
) -> dict[str, tuple[str, str]]:
    """Decompile and recognize AQW's common per-symbol color script."""
    output = destination / (hashlib.sha1(str(source).encode()).hexdigest()[:12])
    command = _ffdec_command(
        ffdec,
        "-export",
        "script",
        str(output),
        str(source),
        home=destination / ".ffdec-home",
    )
    result = subprocess.run(command, capture_output=True, text=True)
    if result.returncode:
        return {}
    rules: dict[str, tuple[str, str]] = {}
    for path in output.rglob("*.as"):
        try:
            text = path.read_text(encoding="utf-8", errors="replace")
        except OSError:
            continue
        color = _COLOR_CALL_RE.search(text)
        class_match = _CLASS_RE.search(text)
        if color is None or class_match is None:
            continue
        package_match = _PACKAGE_RE.search(text)
        package = package_match.group(1) if package_match else ""
        class_name = class_match.group(1)
        full_name = f"{package}.{class_name}" if package else class_name
        rules[full_name.casefold()] = (color.group(1), color.group(2))
    return rules


def _rewrite_references(root: ET.Element, prefix: str) -> None:
    id_map: dict[str, str] = {}
    for element in root.iter():
        old = element.get("id")
        if old:
            new = f"{prefix}_{old}"
            id_map[old] = new
            element.set("id", new)
    if not id_map:
        return
    for element in root.iter():
        for name, value in list(element.attrib.items()):
            if value.startswith("#") and value[1:] in id_map:
                element.set(name, f"#{id_map[value[1:]]}")
                continue
            element.set(
                name,
                _URL_REF_RE.sub(
                    lambda match: f"url(#{id_map.get(match.group(1), match.group(1))})",
                    value,
                ),
            )
        if element.text and "url(#" in element.text:
            element.text = _URL_REF_RE.sub(
                lambda match: f"url(#{id_map.get(match.group(1), match.group(1))})",
                element.text,
            )


def clone_imported_symbol(symbol: ImportedSymbol, key: str) -> ImportedSymbol:
    """Clone one symbol and give every internal SVG id a unique prefix."""
    definitions = [copy.deepcopy(definition) for definition in symbol.definitions]
    root_definition = copy.deepcopy(symbol.definition)
    temporary_root = ET.Element(f"{{{SVG_NS}}}g")
    for definition in definitions:
        temporary_root.append(definition)
    temporary_root.append(root_definition)
    _rewrite_references(temporary_root, f"placed_{key}")
    root_definition.set("id", f"symbol_{key}")
    return ImportedSymbol(
        key,
        root_definition,
        definitions,
        symbol.bounds,
        export_zoom=symbol.export_zoom,
        minimum_stroke_scale=symbol.minimum_stroke_scale,
    )


def _positive_float(value: str | None) -> float | None:
    try:
        parsed = float(str(value))
    except (TypeError, ValueError):
        return None
    return parsed if math.isfinite(parsed) and parsed > 0 else None


def prepare_minimum_strokes(
    symbol: ImportedSymbol,
    *,
    layer_scale: float,
) -> tuple[int, int]:
    """Retain enough FFDec metadata to calibrate Flash hairlines later.

    FFDec expands any sub-pixel Flash stroke to one pixel at its own export
    zoom and records both the marker and authored width. Once the export zoom
    wrapper is removed, that width must be recalculated using the complete
    character holder and final SVG viewport scale.
    """
    if (
        not math.isfinite(symbol.export_zoom)
        or symbol.export_zoom <= 0
        or not math.isfinite(symbol.minimum_stroke_scale)
        or symbol.minimum_stroke_scale <= 0
        or not math.isfinite(layer_scale)
        or layer_scale <= 0
    ):
        return 0, 1

    prepared = 0
    malformed = 0
    for subtree in (*symbol.definitions, symbol.definition):
        for element in subtree.iter():
            if element.get(_FFDEC_SMALL_STROKE, "").casefold() != "true":
                continue
            compensated = _positive_float(element.get("stroke-width"))
            original = _positive_float(element.get(_FFDEC_ORIGINAL_STROKE_WIDTH))
            if compensated is None or original is None:
                malformed += 1
                continue
            element.set(_COMPENSATED_STROKE_WIDTH, f"{compensated:.12g}")
            element.set(
                _AUTHORED_STROKE_WIDTH,
                f"{original / symbol.export_zoom:.12g}",
            )
            element.set(
                _SYMBOL_MINIMUM_STROKE_SCALE,
                f"{symbol.minimum_stroke_scale:.12g}",
            )
            element.set(_LAYER_STROKE_SCALE, f"{layer_scale:.12g}")
            prepared += 1
    return prepared, malformed


def svg_viewport_scale(root: ET.Element) -> float | None:
    """Return uniform CSS-pixels-per-viewBox-unit for an output SVG."""
    raw_viewbox = root.get("viewBox", "")
    try:
        viewbox = [float(value) for value in re.split(r"[\s,]+", raw_viewbox.strip())]
    except ValueError:
        return None
    if (
        len(viewbox) != 4
        or not all(math.isfinite(value) for value in viewbox)
        or viewbox[2] <= 0
        or viewbox[3] <= 0
    ):
        return None
    width = item_renderer.parse_svg_length(root.get("width"))
    height = item_renderer.parse_svg_length(root.get("height"))
    if width is None or height is None or width <= 0 or height <= 0:
        return None
    scale_x = width / viewbox[2]
    scale_y = height / viewbox[3]
    if not math.isclose(scale_x, scale_y, rel_tol=1e-6, abs_tol=1e-9):
        return None
    return math.sqrt(scale_x * scale_y)


def calibrate_minimum_strokes(
    root: ET.Element,
    *,
    minimum_pixels: float = 1.0,
) -> int:
    """Make FFDec-marked strokes match Flash's final one-pixel minimum."""
    viewport_scale = svg_viewport_scale(root)
    if viewport_scale is None or not math.isfinite(minimum_pixels) or minimum_pixels <= 0:
        return 0
    calibrated = 0
    for element in root.iter():
        compensated = _positive_float(element.get(_COMPENSATED_STROKE_WIDTH))
        authored = _positive_float(element.get(_AUTHORED_STROKE_WIDTH))
        symbol_scale = _positive_float(element.get(_SYMBOL_MINIMUM_STROKE_SCALE))
        layer_scale = _positive_float(element.get(_LAYER_STROKE_SCALE))
        if None in (compensated, authored, symbol_scale, layer_scale):
            continue
        assert compensated is not None
        assert authored is not None
        assert symbol_scale is not None
        assert layer_scale is not None
        current_pixels = symbol_scale * layer_scale * viewport_scale
        corrected = max(authored, compensated * minimum_pixels / current_pixels)
        element.set("stroke-width", f"{corrected:.12g}")
        calibrated += 1
    if calibrated:
        root.set("data-aqw-minimum-stroke-width", f"{minimum_pixels:.12g}px")
    return calibrated


def _tint_filter_key(location: str, shade: str) -> str:
    return f"aqw_tint_{location.casefold()}_{shade.casefold()}"


def _apply_color_rules(
    root: ET.Element,
    rules: Mapping[str, tuple[str, str]],
) -> None:
    character_name_attr = f"{{{FFDEC_NS}}}characterName"

    def visit(parent: ET.Element) -> None:
        for index, child in list(enumerate(list(parent))):
            visit(child)
            class_name = child.get(character_name_attr, "").casefold()
            rule = rules.get(class_name)
            if rule is None:
                continue
            wrapper = ET.Element(
                f"{{{SVG_NS}}}g",
                {"filter": f"url(#{_tint_filter_key(*rule)})"},
            )
            parent.remove(child)
            wrapper.append(child)
            parent.insert(index, wrapper)

    visit(root)


def import_ffdec_symbol(
    key: str,
    source_svg: Path,
    *,
    zoom: float,
    color_rules: Mapping[str, tuple[str, str]],
    root_class: str,
) -> ImportedSymbol:
    try:
        tree = ET.parse(source_svg)
    except (OSError, ET.ParseError) as error:
        raise CharacterSvgError(f"Invalid FFDec SVG {source_svg}: {error}") from error
    root = tree.getroot()
    def dimension(name: str) -> float | None:
        value = str(root.get(name, "")).strip()
        match = re.fullmatch(r"([-+0-9.eE]+)(?:px)?", value)
        if match is None:
            return None
        try:
            parsed = float(match.group(1))
        except ValueError:
            return None
        return parsed if math.isfinite(parsed) and parsed >= 0 else None

    width = dimension("width")
    height = dimension("height")
    if width is None or height is None:
        raise CharacterSvgError(f"FFDec SVG has no usable dimensions: {source_svg}")

    definitions: list[ET.Element] = []
    rendered: list[ET.Element] = []
    for child in list(root):
        if child.tag.rsplit("}", 1)[-1] == "defs":
            definitions.extend(list(child))
        else:
            rendered.append(child)
    if width == 0 or height == 0:
        empty = ET.Element(f"{{{SVG_NS}}}g", {"id": f"symbol_{key}"})
        temporary_root = ET.Element(f"{{{SVG_NS}}}g")
        for definition in definitions:
            temporary_root.append(definition)
        temporary_root.append(empty)
        _rewrite_references(temporary_root, f"part_{key}")
        empty.set("id", f"symbol_{key}")
        return ImportedSymbol(
            key,
            empty,
            definitions,
            (0.0, 0.0, 0.0, 0.0),
            export_zoom=zoom,
            minimum_stroke_scale=1 / zoom,
        )
    if len(rendered) != 1:
        raise CharacterSvgError(
            f"Expected one FFDec frame wrapper in {source_svg}, found {len(rendered)}"
        )
    frame = rendered[0]
    export_matrix = parse_matrix(frame.get("transform"))
    if export_matrix is None:
        raise CharacterSvgError(f"FFDec frame wrapper has no matrix: {source_svg}")
    a, b, c, d, e, f = export_matrix
    if (
        abs(b) > 1e-8
        or abs(c) > 1e-8
        or abs(a - zoom) > 1e-5
        or abs(d - zoom) > 1e-5
    ):
        raise CharacterSvgError(
            f"Unexpected FFDec crop/zoom matrix {export_matrix} in {source_svg}"
        )

    # FFDec translates the symbol registration point to a tight positive
    # canvas and puts export zoom on this outer wrapper. Removing the wrapper's
    # matrix restores the authored registration coordinates while preserving
    # FFDec's zoom-dependent stroke widths in the vector definitions.
    frame.attrib.pop("transform", None)
    symbol_bounds = (-e / zoom, -f / zoom, width / zoom, height / zoom)
    root_definition = ET.Element(f"{{{SVG_NS}}}g", {"id": f"symbol_{key}"})
    root_rule = color_rules.get(root_class.casefold())
    if root_rule is not None:
        root_definition.set("filter", f"url(#{_tint_filter_key(*root_rule)})")
    root_definition.append(frame)

    temporary_root = ET.Element(f"{{{SVG_NS}}}g")
    for definition in definitions:
        temporary_root.append(definition)
    temporary_root.append(root_definition)
    _apply_color_rules(temporary_root, color_rules)
    _rewrite_references(temporary_root, f"part_{key}")

    # _rewrite_references renamed the public definition id too. Give it one
    # predictable id for layer references after every internal id is unique.
    root_definition.set("id", f"symbol_{key}")
    return ImportedSymbol(
        key,
        root_definition,
        definitions,
        symbol_bounds,
        export_zoom=zoom,
        minimum_stroke_scale=1 / zoom,
    )


def _rendered_svg_children(root: ET.Element) -> tuple[list[ET.Element], list[ET.Element]]:
    definitions: list[ET.Element] = []
    rendered: list[ET.Element] = []
    for child in list(root):
        if child.tag.rsplit("}", 1)[-1] == "defs":
            definitions.extend(list(child))
        else:
            rendered.append(child)
    return definitions, rendered


def _first_render_transform(root: ET.Element) -> Matrix | None:
    _, rendered = _rendered_svg_children(root)
    if len(rendered) != 1:
        return None
    return parse_matrix(rendered[0].get("transform"))


def _matches_game_space_transform(actual: Matrix, expected: Matrix) -> bool:
    """Whether ``actual`` is an expected holder matrix times inverse zoom."""
    # render_swf_items applies inverse FFDec zoom to the holder's linear part;
    # the holder translation remains unchanged. Solve the common linear ratio
    # and tolerate FFDec/decimal serialization rounding.
    if not math.isclose(actual[4], expected[4], rel_tol=0, abs_tol=2e-4):
        return False
    if not math.isclose(actual[5], expected[5], rel_tol=0, abs_tol=2e-4):
        return False
    if any(
        abs(actual_value) > 2e-5
        for actual_value, expected_value in zip(actual[:4], expected[:4])
        if abs(expected_value) <= 1e-9
    ):
        return False
    ratios = [
        actual_value / expected_value
        for actual_value, expected_value in zip(actual[:4], expected[:4])
        if abs(expected_value) > 1e-9
    ]
    if not ratios or any(ratio <= 0 or not math.isfinite(ratio) for ratio in ratios):
        return False
    ratio = sum(ratios) / len(ratios)
    return all(math.isclose(value, ratio, rel_tol=2e-4, abs_tol=2e-5) for value in ratios)


def inspect_svg_override(
    path: Path,
    *,
    slot: str,
    weapon_type: str,
    requested_space: str = "auto",
) -> tuple[str, float, Matrix]:
    """Identify a reusable SVG as character-space or raw symbol-space.

    Character-space SVGs are the retained ``--game-scale`` vectors produced by
    ``render_swf_items.py``. Raw symbol-space SVGs are direct FFDec frame
    exports whose outer matrix contains only uniform export zoom and crop
    translation. Arbitrary tight SVGs have lost the Flash registration point
    and are rejected rather than being silently misaligned.
    """
    if slot == "armor":
        raise CharacterSvgError(
            "A flat armor SVG cannot be overridden faithfully because AQW "
            "interleaves its head, torso, limbs, and robes with other slots; "
            "use the armor SWF or item ID"
        )
    try:
        root = ET.parse(path).getroot()
    except (OSError, ET.ParseError) as error:
        raise CharacterSvgError(f"Invalid SVG override {path}: {error}") from error
    if root.tag.rsplit("}", 1)[-1] != "svg":
        raise CharacterSvgError(f"SVG override has no <svg> root: {path}")
    if root.get("data-renderer", "").startswith("ffdec-static-compositor"):
        raise CharacterSvgError(
            "--svg expects one item SVG, not a complete composed-character SVG"
        )

    expected = svg_slot_transform(slot, weapon_type)
    outer = _first_render_transform(root)
    declared = root.get("data-aqw-coordinate-space", "").casefold()
    selected = requested_space if requested_space != "auto" else declared or "auto"

    if selected in {"character", "game", "game-space"}:
        if outer is None or not _matches_game_space_transform(outer, expected):
            raise CharacterSvgError(
                f"{path} does not contain the expected {slot} character-space matrix"
            )
        return "character", 1.0, expected

    if selected in {"symbol", "raw", "symbol-space"}:
        if outer is None:
            raise CharacterSvgError(f"{path} has no FFDec symbol registration matrix")
        zoom = (outer[0] + outer[3]) / 2
        if (
            zoom <= 0
            or abs(outer[1]) > 1e-8
            or abs(outer[2]) > 1e-8
            or not math.isclose(outer[0], outer[3], rel_tol=0, abs_tol=1e-6)
        ):
            raise CharacterSvgError(f"{path} is not a raw FFDec symbol SVG")
        return "symbol", zoom, expected

    if outer is not None and _matches_game_space_transform(outer, expected):
        return "character", 1.0, expected
    if outer is not None:
        zoom = (outer[0] + outer[3]) / 2
        if (
            zoom > 0
            and abs(outer[1]) <= 1e-8
            and abs(outer[2]) <= 1e-8
            and math.isclose(outer[0], outer[3], rel_tol=0, abs_tol=1e-6)
        ):
            return "symbol", zoom, expected
    raise CharacterSvgError(
        f"Cannot recover an AQW registration point from {path}. Use a saved "
        "render_swf_items.py --game-scale SVG, a raw FFDec SVG, or the source SWF."
    )


def import_character_space_svg(key: str, source_svg: Path) -> ImportedSymbol:
    """Import a retained game-scale item SVG and restore its registration.

    Older ``render_swf_items.py`` outputs wrap FFDec's cropped frame in the AQW
    holder matrix but leave the nested crop translation in place. That is fine
    for a standalone tight image, but not for putting the item back in a hand.
    Remove that nested translation while retaining its export zoom, then shift
    the known character-space bounds by the same amount.
    """
    try:
        tree = ET.parse(source_svg)
    except (OSError, ET.ParseError) as error:
        raise CharacterSvgError(f"Invalid SVG override {source_svg}: {error}") from error
    root = tree.getroot()
    viewbox = item_renderer.svg_canvas_viewbox(source_svg)
    if viewbox is None:
        raise CharacterSvgError(f"SVG override has no usable viewBox: {source_svg}")
    definitions, rendered = _rendered_svg_children(root)
    if len(rendered) != 1:
        raise CharacterSvgError(
            f"SVG override must contain one rendered top-level group, found "
            f"{len(rendered)}: {source_svg}"
        )
    holder = parse_matrix(rendered[0].get("transform"))
    if holder is None:
        raise CharacterSvgError(
            f"Game-space SVG has no outer AQW holder transform: {source_svg}"
        )
    frame_candidates = [
        child
        for child in list(rendered[0])
        if child.tag.rsplit("}", 1)[-1] != "defs"
    ]
    if len(frame_candidates) != 1:
        raise CharacterSvgError(
            f"Game-space SVG has no unique nested FFDec frame: {source_svg}"
        )
    frame = frame_candidates[0]
    crop = parse_matrix(frame.get("transform"))
    if (
        crop is None
        or crop[0] <= 0
        or abs(crop[1]) > 1e-8
        or abs(crop[2]) > 1e-8
        or not math.isclose(crop[0], crop[3], rel_tol=0, abs_tol=1e-6)
    ):
        raise CharacterSvgError(
            f"Game-space SVG has no recoverable FFDec crop matrix: {source_svg}"
        )
    frame.set("transform", matrix_text((crop[0], 0.0, 0.0, crop[3], 0.0, 0.0)))
    shift_x = holder[0] * crop[4] + holder[2] * crop[5]
    shift_y = holder[1] * crop[4] + holder[3] * crop[5]
    viewbox = (viewbox[0] - shift_x, viewbox[1] - shift_y, viewbox[2], viewbox[3])
    root_definition = ET.Element(f"{{{SVG_NS}}}g", {"id": f"symbol_{key}"})
    for child in rendered:
        root_definition.append(child)
    temporary_root = ET.Element(f"{{{SVG_NS}}}g")
    for definition in definitions:
        temporary_root.append(definition)
    temporary_root.append(root_definition)
    _rewrite_references(temporary_root, f"part_{key}")
    root_definition.set("id", f"symbol_{key}")
    return ImportedSymbol(
        key,
        root_definition,
        definitions,
        viewbox,
        export_zoom=crop[0],
        minimum_stroke_scale=item_renderer.affine_geometric_scale(holder),
    )


def tint_rgb(color: int, shade: str) -> tuple[int, int, int]:
    red = (color >> 16) & 0xFF
    green = (color >> 8) & 0xFF
    blue = color & 0xFF
    normalized = shade.casefold()
    if normalized == "light":
        offsets = (100, 100, 100)
    elif normalized == "dark":
        offsets = (-25, -50, -50)
    elif normalized == "darker":
        offsets = (-125, -125, -125)
    else:
        offsets = (0, 0, 0)
    return tuple(
        max(0, min(255, component + offset))
        for component, offset in zip((red, green, blue), offsets)
    )  # type: ignore[return-value]


def add_color_filters(
    defs: ET.Element,
    rules: Iterable[tuple[str, str]],
    fields: Mapping[str, str],
) -> list[str]:
    warnings: list[str] = []
    for location, shade in sorted(set(rules), key=lambda pair: (pair[0].casefold(), pair[1].casefold())):
        raw = fields.get(f"intColor{location}")
        try:
            color = int(str(raw))
        except (TypeError, ValueError):
            warnings.append(f"Missing intColor{location}; left its authored colors unchanged")
            color = None
        filter_element = ET.SubElement(
            defs,
            f"{{{SVG_NS}}}filter",
            {
                "id": _tint_filter_key(location, shade),
                "x": "-100%",
                "y": "-100%",
                "width": "300%",
                "height": "300%",
                "color-interpolation-filters": "sRGB",
            },
        )
        if color is None:
            values = "1 0 0 0 0 0 1 0 0 0 0 0 1 0 0 0 0 0 1 0"
        else:
            red, green, blue = tint_rgb(color, shade)
            values = (
                f"0 0 0 0 {red / 255:.9g} "
                f"0 0 0 0 {green / 255:.9g} "
                f"0 0 0 0 {blue / 255:.9g} "
                "0 0 0 1 0"
            )
        ET.SubElement(
            filter_element,
            f"{{{SVG_NS}}}feColorMatrix",
            {"type": "matrix", "values": values},
        )
    dark = ET.SubElement(
        defs,
        f"{{{SVG_NS}}}filter",
        {
            "id": "aqw_back_part_dark",
            "x": "-100%",
            "y": "-100%",
            "width": "300%",
            "height": "300%",
            "color-interpolation-filters": "sRGB",
        },
    )
    ET.SubElement(
        dark,
        f"{{{SVG_NS}}}feColorMatrix",
        {"type": "matrix", "values": "0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 1 0"},
    )
    return warnings


def build_layers(
    aliases: Mapping[str, str],
    *,
    weapon_type: str,
) -> list[Layer]:
    """Build the exact back-to-front idle display order."""
    layers: list[Layer] = []

    def add(name: str, alias: str, transform: str, *, darken: bool = False) -> None:
        key = aliases.get(alias)
        if key is not None:
            layers.append(Layer(name, key, PART_TRANSFORMS[transform], darken))

    add("ground", "ground", "ground")
    add("backhair", "backhair", "backhair")
    add("cape", "cape", "cape")

    normalized_weapon_type = weapon_type.casefold()
    if normalized_weapon_type == "dagger":
        # Stock characterB puts weaponOff below the cape. Hero/in-game output
        # draws the rear-hand weapon immediately in front of the cape.
        add("weapon_off", "weapon", "weapon_off")

    add("back_shoulder", "shoulder", "back_shoulder", darken=True)
    add("back_hand", "hand", "back_hand", darken=True)
    if normalized_weapon_type == "gauntlet":
        add("gauntlet_back", "weapon", "gauntlet_back")
    add("back_robe", "back_robe", "back_robe")
    add("back_foot", "back_foot", "back_foot", darken=True)
    add("back_thigh", "thigh", "back_thigh", darken=True)
    add("chest", "chest", "chest")
    add("hip", "hip", "hip")
    add("back_shin", "shin", "back_shin", darken=True)
    add("head", "head", "head")
    add("hair", "hair", "helm")
    add("helm", "helm", "helm")
    add("front_thigh", "thigh", "front_thigh")
    add("front_shin", "shin", "front_shin")
    add("idle_foot", "idle_foot", "idle_foot")
    add("robe", "robe", "robe")
    if normalized_weapon_type != "gauntlet":
        add("weapon", "weapon", "weapon")
    add("front_shoulder", "shoulder", "front_shoulder")
    add("front_hand", "hand", "front_hand")
    if normalized_weapon_type == "gauntlet":
        add("gauntlet_front", "weapon", "gauntlet_front")
    return layers


def rebase_character_space_override(
    layers: Sequence[Layer],
    *,
    symbol_key: str,
    source_transform: Matrix,
) -> list[Layer]:
    """Replace holder transforms for artwork already in character coordinates."""
    inverse_source = invert_transform(source_transform)
    return [
        Layer(
            layer.name,
            layer.symbol_key,
            item_renderer.compose_transforms(layer.transform, inverse_source),
            layer.darken,
        )
        if layer.symbol_key == symbol_key
        else layer
        for layer in layers
    ]


def _transformed_bounds(
    bounds: tuple[float, float, float, float],
    matrix: Matrix,
) -> tuple[float, float, float, float]:
    x, y, width, height = bounds
    points = [
        item_renderer.transform_point(matrix, px, py)
        for px, py in (
            (x, y),
            (x + width, y),
            (x, y + height),
            (x + width, y + height),
        )
    ]
    min_x = min(point[0] for point in points)
    min_y = min(point[1] for point in points)
    max_x = max(point[0] for point in points)
    max_y = max(point[1] for point in points)
    return min_x, min_y, max_x - min_x, max_y - min_y


def _union_bounds(bounds: Sequence[tuple[float, float, float, float]]) -> tuple[float, float, float, float]:
    if not bounds:
        raise CharacterSvgError("Character composition produced no visible layers")
    min_x = min(value[0] for value in bounds)
    min_y = min(value[1] for value in bounds)
    max_x = max(value[0] + value[2] for value in bounds)
    max_y = max(value[1] + value[3] for value in bounds)
    return min_x, min_y, max_x - min_x, max_y - min_y


def _set_output_geometry(
    root: ET.Element,
    viewbox: tuple[float, float, float, float],
    *,
    max_size: int,
) -> None:
    x, y, width, height = viewbox
    scale = max_size / max(width, height)
    root.set("viewBox", f"{x:.9g} {y:.9g} {width:.9g} {height:.9g}")
    root.set("width", f"{width * scale:.9g}px")
    root.set("height", f"{height * scale:.9g}px")


def unresolved_svg_references(root: ET.Element) -> set[str]:
    """Return fragment ids referenced by the SVG but never defined."""
    ids = {element.get("id") for element in root.iter() if element.get("id")}
    references: set[str] = set()
    for element in root.iter():
        for name, value in element.attrib.items():
            if name.endswith("href") and value.startswith("#"):
                references.add(value[1:])
            references.update(_URL_REF_RE.findall(value))
        if element.text:
            references.update(_URL_REF_RE.findall(element.text))
    return references.difference(ids)


def compose_svg(
    imported: Mapping[str, ImportedSymbol],
    layers: Sequence[Layer],
    *,
    fields: Mapping[str, str],
    all_color_rules: Iterable[tuple[str, str]],
    output: Path,
    max_size: int,
    padding: int,
    facing: str,
    rsvg_convert: str | None,
) -> list[str]:
    root = ET.Element(
        f"{{{SVG_NS}}}svg",
        {
            "version": "1.1",
            f"{{{FFDEC_NS}}}objectType": "aqw-character",
            "data-renderer": "ffdec-static-compositor-v1",
        },
    )
    defs = ET.SubElement(root, f"{{{SVG_NS}}}defs")
    warnings = add_color_filters(defs, all_color_rules, fields)

    # The AvatarMC local display list (also used by Hero/native try-on output)
    # faces right. Some characterB page timelines mirror that complete object,
    # but that parent-page presentation transform is not part of the character.
    direction = 1.0 if facing == "right" else -1.0
    outer: Matrix = (
        direction * CHARACTER_DISPLAY_SCALE,
        0.0,
        0.0,
        CHARACTER_DISPLAY_SCALE,
        0.0,
        0.0,
    )
    layer_group = ET.SubElement(root, f"{{{SVG_NS}}}g", {"id": "aqw-character"})
    computed_bounds: list[tuple[float, float, float, float]] = []
    prepared_strokes = 0
    malformed_strokes = 0
    for layer_index, layer in enumerate(layers):
        source_symbol = imported[layer.symbol_key]
        matrix = item_renderer.compose_transforms(outer, layer.transform)
        placed_key = f"{layer_index:02d}_{layer.name}"
        symbol = clone_imported_symbol(source_symbol, placed_key)
        prepared, malformed = prepare_minimum_strokes(
            symbol,
            layer_scale=item_renderer.affine_geometric_scale(matrix),
        )
        prepared_strokes += prepared
        malformed_strokes += malformed
        for definition in symbol.definitions:
            defs.append(definition)
        defs.append(symbol.definition)
        attributes = {
            "id": f"layer-{layer.name}",
            f"{{{XLINK_NS}}}href": f"#symbol_{placed_key}",
            "href": f"#symbol_{placed_key}",
            "transform": matrix_text(matrix),
        }
        if layer.darken:
            attributes["filter"] = "url(#aqw_back_part_dark)"
        ET.SubElement(layer_group, f"{{{SVG_NS}}}use", attributes)
        computed_bounds.append(_transformed_bounds(source_symbol.bounds, matrix))

    if prepared_strokes:
        root.set("data-aqw-calibrated-minimum-strokes", str(prepared_strokes))
    if malformed_strokes:
        warnings.append(
            f"Could not calibrate {malformed_strokes} malformed FFDec minimum stroke(s)"
        )

    initial = _union_bounds(computed_bounds)
    # Give filters a conservative initial page. A raster alpha probe below can
    # then tighten it without clipping glows at the first render.
    margin = max(initial[2], initial[3]) * 0.1 + 2
    initial = (
        initial[0] - margin,
        initial[1] - margin,
        initial[2] + margin * 2,
        initial[3] + margin * 2,
    )
    _set_output_geometry(root, initial, max_size=max_size)
    calibrate_minimum_strokes(root)
    output.parent.mkdir(parents=True, exist_ok=True)
    tree = ET.ElementTree(root)
    tree.write(output, encoding="utf-8", xml_declaration=True)

    visible = None
    if rsvg_convert:
        visible = item_renderer.detect_svg_visible_viewbox(
            output, rsvg_convert, probe_size=max(1024, max_size * 2), padding_pixels=1
        )
    if visible is None:
        visible = _union_bounds(computed_bounds)
        if rsvg_convert:
            warnings.append("Could not alpha-probe combined SVG; used vector bounds")

    content_max = max(visible[2], visible[3])
    content_pixels = max(1, max_size - padding * 2)
    units_per_pixel = content_max / content_pixels
    padding_units = padding * units_per_pixel
    final = (
        visible[0] - padding_units,
        visible[1] - padding_units,
        visible[2] + padding_units * 2,
        visible[3] + padding_units * 2,
    )
    _set_output_geometry(root, final, max_size=max_size)
    calibrate_minimum_strokes(root)
    unresolved = sorted(unresolved_svg_references(root))
    if unresolved:
        preview = ", ".join(unresolved[:5])
        if len(unresolved) > 5:
            preview += ", ..."
        warnings.append(
            f"FFDec emitted {len(unresolved)} unresolved SVG reference(s): {preview}"
        )
    tree.write(output, encoding="utf-8", xml_declaration=True)
    return warnings


def render_preview(svg: Path, png: Path, *, max_size: int, rsvg_convert: str) -> None:
    png.parent.mkdir(parents=True, exist_ok=True)
    rendered = item_renderer.render_svg_to_maximum(svg, png, max_size, rsvg_convert)
    if rendered is None:
        raise CharacterSvgError(f"Unable to rasterize SVG preview: {svg}")


def numbered_output_paths(base: Path, frame_count: int) -> list[Path]:
    """Return one original path or a stable, zero-padded frame sequence."""
    if frame_count < 1:
        raise CharacterSvgError("Frame count must be positive")
    if frame_count == 1:
        return [base]
    width = max(3, len(str(frame_count)))
    return [
        base.with_name(f"{base.stem}-{index:0{width}d}{base.suffix}")
        for index in range(1, frame_count + 1)
    ]


def align_frame_svg_viewboxes(
    paths: Sequence[Path],
    *,
    max_size: int,
    padding: int,
) -> tuple[float, float, float, float]:
    """Give a sequence one shared character-space canvas to prevent jitter."""
    viewboxes = []
    for path in paths:
        viewbox = item_renderer.svg_canvas_viewbox(path)
        if viewbox is None:
            raise CharacterSvgError(f"Generated SVG has no usable viewBox: {path}")
        viewboxes.append(viewbox)
    combined = _union_bounds(viewboxes)
    content_pixels = max(1, max_size - padding * 2)
    units_per_pixel = max(combined[2], combined[3]) / content_pixels
    padding_units = padding * units_per_pixel
    shared = (
        combined[0] - padding_units,
        combined[1] - padding_units,
        combined[2] + padding_units * 2,
        combined[3] + padding_units * 2,
    )
    for path in paths:
        try:
            tree = ET.parse(path)
        except (OSError, ET.ParseError) as error:
            raise CharacterSvgError(f"Cannot align generated SVG {path}: {error}") from error
        _set_output_geometry(tree.getroot(), shared, max_size=max_size)
        calibrate_minimum_strokes(tree.getroot())
        tree.write(path, encoding="utf-8", xml_declaration=True)
    return shared


def expanded_frame_durations(
    frame_count: int,
    frame_durations: int | Sequence[int],
) -> list[int]:
    if isinstance(frame_durations, int):
        durations = [frame_durations] * frame_count
    else:
        durations = list(frame_durations)
    if len(durations) != frame_count:
        raise CharacterSvgError(
            f"Animation has {frame_count} frames but {len(durations)} durations"
        )
    if any(duration < 1 for duration in durations):
        raise CharacterSvgError("Animation frame durations must be positive")
    return durations


def _union_pixel_boxes(
    boxes: Iterable[tuple[int, int, int, int] | None],
) -> tuple[int, int, int, int] | None:
    present = [box for box in boxes if box is not None]
    if not present:
        return None
    return (
        min(box[0] for box in present),
        min(box[1] for box in present),
        max(box[2] for box in present),
        max(box[3] for box in present),
    )


def animation_delta_crop(
    current_path: Path,
    previous_path: Path | None,
) -> tuple[int, int, int, int, tuple[int, int]]:
    """Return an even-offset crop containing every changed RGBA pixel."""
    try:
        with Image.open(current_path) as current_image:
            current = current_image.convert("RGBA")
        size = current.size
        if previous_path is None:
            return 0, 0, size[0], size[1], size
        with Image.open(previous_path) as previous_image:
            previous = previous_image.convert("RGBA")
    except OSError as error:
        raise CharacterSvgError(f"Cannot inspect animation PNG: {error}") from error
    if previous.size != size:
        raise CharacterSvgError("Animation PNG frames do not share one canvas size")
    difference = ImageChops.difference(previous, current)
    bounds = _union_pixel_boxes(channel.getbbox() for channel in difference.split())
    if bounds is None:
        return 0, 0, 1, 1, size
    # Animated WebP requires even x/y frame offsets. Expand left/up as needed
    # while retaining the complete difference rectangle.
    left = bounds[0] & ~1
    top = bounds[1] & ~1
    return left, top, bounds[2] - left, bounds[3] - top, size


def save_animation_webp_pillow(
    png_paths: Sequence[Path],
    output: Path,
    *,
    frame_durations: int | Sequence[int],
    method: int,
    lossy_quality: float | None,
) -> None:
    frames: list[Image.Image] = []
    for path in png_paths:
        try:
            with Image.open(path) as image:
                frames.append(image.convert("RGBA"))
        except OSError as error:
            raise CharacterSvgError(f"Cannot read animation frame {path}: {error}") from error
    if not frames:
        raise CharacterSvgError("Animation contains no PNG frames")
    if len({frame.size for frame in frames}) != 1:
        raise CharacterSvgError("Animation PNG frames do not share one canvas size")
    durations = expanded_frame_durations(len(frames), frame_durations)
    output.parent.mkdir(parents=True, exist_ok=True)
    compression_options: dict[str, object] = (
        {"lossless": True}
        if lossy_quality is None
        else {"lossless": False, "quality": lossy_quality}
    )
    try:
        if len(frames) == 1:
            frames[0].save(
                output,
                format="WEBP",
                method=method,
                **compression_options,
            )
        else:
            frames[0].save(
                output,
                save_all=True,
                append_images=frames[1:],
                duration=durations,
                loop=0,
                method=method,
                format="WEBP",
                **compression_options,
            )
    finally:
        for frame in frames:
            frame.close()


def save_animation_webp_parallel(
    png_paths: Sequence[Path],
    output: Path,
    *,
    frame_durations: int | Sequence[int],
    workers: int,
    method: int,
    lossy_quality: float | None,
    cwebp: str,
    webpmux: str,
) -> None:
    """Encode delta frames concurrently, then losslessly mux the animation."""
    if not png_paths:
        raise CharacterSvgError("Animation contains no PNG frames")
    durations = expanded_frame_durations(len(png_paths), frame_durations)
    output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="aqw-character-webp-") as temporary:
        work_dir = Path(temporary)

        def encode_frame(index: int) -> tuple[Path, int, int]:
            current = png_paths[index]
            previous = png_paths[index - 1] if index else None
            x, y, width, height, canvas = animation_delta_crop(current, previous)
            encoded = work_dir / f"frame-{index + 1:06d}.webp"
            command = [
                cwebp,
                "-quiet",
            ]
            if lossy_quality is None:
                command.extend(("-lossless", "-exact"))
            else:
                command.extend(
                    ("-q", f"{lossy_quality:g}", "-alpha_q", "100")
                )
            command.extend(("-m", str(method)))
            if (x, y, width, height) != (0, 0, canvas[0], canvas[1]):
                command.extend(
                    ("-crop", str(x), str(y), str(width), str(height))
                )
            command.extend((str(current), "-o", str(encoded)))
            result = subprocess.run(command, capture_output=True, text=True)
            if result.returncode:
                detail = (result.stderr or result.stdout).strip()
                raise CharacterSvgError(
                    f"cwebp failed for {current}: {detail[-2000:]}"
                )
            return encoded, x, y

        if workers == 1 or len(png_paths) == 1:
            encoded_frames = [encode_frame(index) for index in range(len(png_paths))]
        else:
            with ThreadPoolExecutor(max_workers=workers) as executor:
                encoded_frames = list(executor.map(encode_frame, range(len(png_paths))))

        command = [webpmux]
        for (encoded, x, y), duration in zip(encoded_frames, durations):
            command.extend(
                ("-frame", str(encoded), f"+{duration}+{x}+{y}+0-b")
            )
        command.extend(
            ("-loop", "0", "-bgcolor", "0,0,0,0", "-o", str(output))
        )
        result = subprocess.run(command, capture_output=True, text=True)
        if result.returncode:
            detail = (result.stderr or result.stdout).strip()
            raise CharacterSvgError(f"webpmux failed: {detail[-2000:]}")


def save_flashvars(fields: Mapping[str, str], output: Path) -> None:
    """Persist a public character response for deterministic offline reruns."""
    output = output.expanduser().resolve()
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(
        json.dumps({"flashvars": dict(sorted(fields.items()))}, indent=2) + "\n",
        encoding="utf-8",
    )


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description=(
            "Compose an AQW character (armor, weapon, helm/hair, cape, and ground) "
            "as one or more SVG frames using FFDec only."
        )
    )
    parser.add_argument("username", help="Public AQW character name")
    parser.add_argument(
        "--flashvars-json",
        type=Path,
        help="Use saved character FlashVars instead of fetching the public page",
    )
    parser.add_argument(
        "--save-flashvars-json",
        type=Path,
        help="Save the fetched/loaded character data for later offline reruns",
    )
    parser.add_argument(
        "--base-items",
        action="store_true",
        help="Use equipped base items instead of cosmetic overrides",
    )
    parser.add_argument(
        "--show-hidden",
        action="store_true",
        help="Include equipped cape/helm even when the character has hidden them",
    )
    source = parser.add_mutually_exclusive_group()
    source.add_argument("--swf", type=Path, help="Override one slot with a local SWF")
    source.add_argument("--item-id", type=int, help="Override one slot using item_db.json")
    source.add_argument(
        "--svg",
        type=Path,
        help="Override one non-armor slot with a reusable item SVG",
    )
    parser.add_argument("--slot", choices=sorted(SUPPORTED_OVERRIDE_SLOTS))
    parser.add_argument("--link", help="Override SWF exported class (normally inferred)")
    parser.add_argument("--name", help="Override item display name")
    parser.add_argument(
        "--weapon-type",
        choices=("Sword", "Dagger", "Gauntlet"),
        help="Override weapon holder behavior",
    )
    parser.add_argument(
        "--svg-space",
        choices=("auto", "character", "symbol"),
        default="auto",
        help=(
            "SVG coordinate space: detect automatically, retained game-scale item SVG, "
            "or raw FFDec symbol SVG"
        ),
    )
    parser.add_argument("--output", type=Path, help="Output SVG path")
    parser.add_argument("--preview-png", type=Path, help="Optional raster preview path")
    timeline = parser.add_mutually_exclusive_group()
    timeline.add_argument(
        "--frames",
        type=int,
        help="Fixed number of nested timeline frames to export (default: 1)",
    )
    timeline.add_argument(
        "--complete-loop",
        action="store_true",
        help="Detect and export one complete combined nested-animation loop",
    )
    parser.add_argument(
        "--max-frames",
        type=int,
        default=DEFAULT_LOOP_MAX_FRAMES,
        help=(
            "Per-timeline detection limit and output cap for --complete-loop "
            f"(default: {DEFAULT_LOOP_MAX_FRAMES})"
        ),
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help=(
            "With --complete-loop, print its full frame count without composing "
            "SVG, PNG, or WebP outputs"
        ),
    )
    parser.add_argument(
        "--subframe-start",
        type=int,
        default=1,
        help="First nested timeline frame to export, one-indexed (default: 1)",
    )
    parser.add_argument(
        "--animation-webp",
        type=Path,
        help="Optional animated WebP assembled from --preview-png frames",
    )
    parser.add_argument(
        "--webp-encoder",
        choices=("auto", "parallel", "pillow"),
        default="auto",
        help=(
            "WebP encoder: parallel cwebp/webpmux when available, "
            "portable Pillow, or automatic selection (default: auto)"
        ),
    )
    parser.add_argument(
        "--webp-method",
        type=int,
        default=4,
        metavar="0-6",
        help="WebP compression effort, 0 fastest and 6 smallest (default: 4)",
    )
    parser.add_argument(
        "--webp-lossy-quality",
        type=float,
        metavar="0-100",
        help=(
            "Enable lossy WebP RGB compression at this quality; alpha remains "
            "lossless (recommended: 85, default: exact lossless RGB)"
        ),
    )
    parser.add_argument(
        "--frame-duration",
        type=int,
        metavar="MS",
        help=(
            "Override animated WebP milliseconds per frame; by default timing "
            "comes from characterB.swf's frame rate"
        ),
    )
    parser.add_argument("--max-size", type=int, default=512)
    parser.add_argument("--padding", type=int, default=0)
    parser.add_argument(
        "--workers",
        type=int,
        default=min(4, os.cpu_count() or 1),
        help="Concurrent SVG composition and PNG raster workers (default: up to 4)",
    )
    parser.add_argument(
        "--zoom",
        type=float,
        default=1.0,
        help="FFDec SVG export zoom/stroke interpretation (default: 1)",
    )
    parser.add_argument("--facing", choices=("right", "left"), default="right")
    parser.add_argument("--timeout", type=float, default=15.0)
    parser.add_argument("--offline", action="store_true", help="Never download missing SWFs")
    parser.add_argument("--asset-root", type=Path, default=tryon.DEFAULT_ASSET_ROOT)
    parser.add_argument("--asset-cache", type=Path, default=DEFAULT_ASSET_CACHE)
    parser.add_argument("--database", type=Path, default=tryon.DEFAULT_DATABASE)
    parser.add_argument("--ffdec", type=Path, default=DEFAULT_FFDEC)
    parser.add_argument("--character-renderer", type=Path, default=DEFAULT_CHARACTER_RENDERER)
    parser.add_argument("--rsvg-convert", help="Path to rsvg-convert (found on PATH by default)")
    parser.add_argument(
        "--no-color-customization",
        action="store_true",
        help="Do not translate mcSetColor frame scripts into SVG filters",
    )
    parser.add_argument("--keep-parts", type=Path, help="Keep raw FFDec part exports here")
    return parser


def run(args: argparse.Namespace) -> Path:
    if args.max_size < 64:
        raise CharacterSvgError("--max-size must be at least 64")
    if args.padding < 0 or args.padding * 2 >= args.max_size:
        raise CharacterSvgError("--padding must be nonnegative and less than half max size")
    if args.workers < 1:
        raise CharacterSvgError("--workers must be positive")
    if not math.isfinite(args.zoom) or args.zoom <= 0:
        raise CharacterSvgError("--zoom must be a finite positive number")
    if args.frames is not None and args.frames < 1:
        raise CharacterSvgError("--frames must be positive")
    if args.max_frames < 1 or args.subframe_start < 1:
        raise CharacterSvgError("--max-frames and --subframe-start must be positive")
    if args.frame_duration is not None and args.frame_duration < 1:
        raise CharacterSvgError("--frame-duration must be positive")
    if not 0 <= args.webp_method <= 6:
        raise CharacterSvgError("--webp-method must be between 0 and 6")
    if args.webp_lossy_quality is not None and (
        not math.isfinite(args.webp_lossy_quality)
        or not 0 <= args.webp_lossy_quality <= 100
    ):
        raise CharacterSvgError("--webp-lossy-quality must be between 0 and 100")
    if args.dry_run and not args.complete_loop:
        raise CharacterSvgError("--dry-run requires --complete-loop")
    if args.dry_run and args.keep_parts is not None:
        raise CharacterSvgError("--dry-run cannot be combined with --keep-parts")
    if args.animation_webp is not None and args.preview_png is None:
        raise CharacterSvgError("--animation-webp requires --preview-png")

    ffdec = args.ffdec.expanduser().resolve()
    character_renderer = args.character_renderer.expanduser().resolve()
    cwebp = shutil.which("cwebp")
    webpmux = shutil.which("webpmux")
    if not ffdec.is_file():
        raise CharacterSvgError(f"FFDec CLI jar does not exist: {ffdec}")
    if not tryon.has_valid_swf_header(character_renderer):
        raise CharacterSvgError(f"characterB.swf is missing or invalid: {character_renderer}")
    if (
        args.animation_webp is not None
        and args.webp_encoder == "parallel"
        and (cwebp is None or webpmux is None)
    ):
        raise CharacterSvgError(
            "--webp-encoder parallel requires cwebp and webpmux from libwebp"
        )

    fields = (
        load_flashvars(args.flashvars_json.expanduser().resolve())
        if args.flashvars_json is not None
        else tryon.fetch_character_flashvars(args.username, timeout=args.timeout)
    )
    fields = {str(key): str(value) for key, value in fields.items()}
    if args.save_flashvars_json is not None and not args.dry_run:
        save_flashvars(fields, args.save_flashvars_json)
    svg_override_path = args.svg.expanduser().resolve() if args.svg is not None else None
    svg_override_slot = args.slot if svg_override_path is not None else None
    if svg_override_path is not None:
        if svg_override_slot is None:
            raise CharacterSvgError("--slot is required with --svg")
        if not svg_override_path.is_file():
            raise CharacterSvgError(f"SVG override does not exist: {svg_override_path}")
        if svg_override_slot in {"cape", "helm"}:
            bit = 0 if svg_override_slot == "cape" else 1
            fields["ia1"] = str(_visibility_flags(fields) & ~(1 << bit))
    if args.show_hidden:
        fields["ia1"] = str(_visibility_flags(fields) & ~0b111)
    resolver = tryon.LocalAssetResolver(args.asset_root.expanduser().resolve())
    explicit: dict[str, Path] = {}
    override = _resolve_override(args, fields, resolver)
    if override is not None:
        slot, source, link, name, weapon_type = override
        synthetic = _apply_override(
            fields,
            slot=slot,
            source=source,
            link=link,
            name=name,
            weapon_type=weapon_type,
            custom=not args.base_items,
        )
        explicit[synthetic.casefold()] = source

    assets = appearance_assets(fields, use_cosmetics=not args.base_items)
    svg_weapon_type = args.weapon_type or "Sword"
    svg_inspection: tuple[str, float, Matrix] | None = None
    if svg_override_path is not None and svg_override_slot is not None:
        svg_inspection = inspect_svg_override(
            svg_override_path,
            slot=svg_override_slot,
            weapon_type=svg_weapon_type,
            requested_space=args.svg_space,
        )
        assets.pop(svg_override_slot, None)
        if svg_override_slot == "helm":
            assets.pop("hair", None)
    sources: dict[str, Path] = {}
    cache_dir = args.asset_cache.expanduser().resolve()
    for slot, asset in assets.items():
        sources[slot] = resolve_asset_source(
            asset,
            resolver=resolver,
            explicit=explicit,
            cache_dir=cache_dir,
            timeout=args.timeout,
            offline=args.offline,
        )

    gender = fields.get("strGender", "M").upper()
    requests, aliases, warnings = build_symbol_requests(
        assets,
        sources,
        character_renderer=character_renderer,
        gender=gender,
    )
    weapon_type = assets.get("weapon", AppearanceAsset("", "", "", "Sword")).weapon_type
    if svg_override_slot == "weapon":
        weapon_type = svg_weapon_type
    output = args.output
    if output is None:
        name = tryon.sanitize_filename(fields.get("strName") or args.username)
        output = DEFAULT_OUTPUT_DIR / f"{name}.svg"
    output = output.expanduser().resolve()
    fixed_frame_count = args.frames if args.frames is not None else 1
    export_frame_count = (
        args.max_frames + min(LOOP_VALIDATION_FRAMES, args.max_frames)
        if args.complete_loop
        else fixed_frame_count
    )
    detected_loop: int | None = None
    ignored_loop_keys: tuple[str, ...] = ()
    frame_count = fixed_frame_count
    output_paths: list[Path] = []
    preview_paths: list[Path] = []

    temporary_context = None
    if args.keep_parts is not None:
        work_dir = args.keep_parts.expanduser().resolve()
        work_dir.mkdir(parents=True, exist_ok=True)
    else:
        temporary_context = tempfile.TemporaryDirectory(prefix="aqw-character-svg-")
        work_dir = Path(temporary_context.name)

    try:
        raw_exports = export_requested_symbol_frames(
            requests,
            ffdec=ffdec,
            zoom=args.zoom,
            destination=work_dir / "svg",
            subframe_start=args.subframe_start,
            frame_count=export_frame_count,
        )
        if args.complete_loop:
            loop_exports, ignored_loop_keys = loop_driver_exports(raw_exports)
            detected_loop = detect_complete_loop_frame_count(
                loop_exports,
                max_frames=args.max_frames,
            )
            frame_count = (
                min(detected_loop, args.max_frames)
                if detected_loop
                else args.max_frames
            )
            if detected_loop is None:
                warnings.append(
                    "At least one nested timeline did not repeat within the "
                    f"{args.max_frames}-frame scan cap; the output is capped"
                )
            elif detected_loop > args.max_frames:
                warnings.append(
                    f"The complete nested loop is {detected_loop} frames; "
                    f"the output is capped at {args.max_frames}"
                )
        output_paths = numbered_output_paths(output, frame_count)
        preview_paths = (
            numbered_output_paths(
                args.preview_png.expanduser().resolve(),
                frame_count,
            )
            if args.preview_png is not None
            else []
        )
        if args.dry_run:
            character_rate = swf_frame_rate(character_renderer)
            if detected_loop is None:
                print(
                    "Could not determine the complete nested loop because at least "
                    f"one timeline did not repeat within {args.max_frames} frames "
                    f"({character_rate:g} FPS)."
                )
            else:
                duration = detected_loop / character_rate
                print(
                    f"Complete nested loop: {detected_loop} frames at "
                    f"{character_rate:g} FPS ({duration:.3f} seconds)"
                    + (
                        f"; exceeds the {args.max_frames}-frame output cap."
                        if detected_loop > args.max_frames
                        else "."
                    )
                )
            if ignored_loop_keys:
                print(
                    "Ignored character blink timeline(s): "
                    + ", ".join(ignored_loop_keys)
                    + "."
                )
            return output_paths[0]
        rules_by_source: dict[Path, dict[str, tuple[str, str]]] = {}
        if not args.no_color_customization:
            for source in sorted({request.source for request in requests}):
                rules_by_source[source] = parse_color_scripts(
                    source,
                    ffdec=ffdec,
                    destination=work_dir / "scripts",
                )
        used_rules = {
            rule
            for rules in rules_by_source.values()
            for rule in rules.values()
        }

        svg_override_key = "svg_override"
        if svg_override_slot is not None:
            aliases[svg_override_slot] = svg_override_key
        layers = build_layers(aliases, weapon_type=weapon_type)
        if svg_inspection is not None and svg_inspection[0] == "character":
            layers = rebase_character_space_override(
                layers,
                symbol_key=svg_override_key,
                source_transform=svg_inspection[2],
            )
        rsvg_convert = args.rsvg_convert or shutil.which("rsvg-convert")

        def compose_frame(task: tuple[int, Path]) -> list[str]:
            frame_index, frame_output = task
            imported: dict[str, ImportedSymbol] = {}
            for request in requests:
                rules = rules_by_source.get(request.source, {})
                imported[request.key] = import_ffdec_symbol(
                    request.key,
                    raw_exports[request.key][frame_index],
                    zoom=args.zoom,
                    color_rules=rules,
                    root_class=request.class_name,
                )
            if (
                svg_override_path is not None
                and svg_override_slot is not None
                and svg_inspection is not None
            ):
                coordinate_space, svg_zoom, _ = svg_inspection
                if coordinate_space == "character":
                    imported[svg_override_key] = import_character_space_svg(
                        svg_override_key, svg_override_path
                    )
                else:
                    imported[svg_override_key] = import_ffdec_symbol(
                        svg_override_key,
                        svg_override_path,
                        zoom=svg_zoom,
                        color_rules={},
                        root_class="",
                    )
            return compose_svg(
                imported,
                layers,
                fields=fields,
                all_color_rules=used_rules,
                output=frame_output,
                max_size=args.max_size,
                padding=0 if frame_count > 1 else args.padding,
                facing=args.facing,
                rsvg_convert=rsvg_convert,
            )

        frame_tasks = list(enumerate(output_paths))
        if args.workers == 1 or len(frame_tasks) == 1:
            frame_warning_groups = [compose_frame(task) for task in frame_tasks]
        else:
            with ThreadPoolExecutor(max_workers=args.workers) as executor:
                frame_warning_groups = list(executor.map(compose_frame, frame_tasks))
        for frame_warnings in frame_warning_groups:
            warnings.extend(warning for warning in frame_warnings if warning not in warnings)

        if frame_count > 1:
            align_frame_svg_viewboxes(
                output_paths,
                max_size=args.max_size,
                padding=args.padding,
            )
        if preview_paths:
            if not rsvg_convert:
                raise CharacterSvgError("--preview-png requires rsvg-convert")

            def rasterize_frame(task: tuple[Path, Path]) -> None:
                frame_output, preview_output = task
                render_preview(
                    frame_output,
                    preview_output,
                    max_size=args.max_size,
                    rsvg_convert=rsvg_convert,
                )

            raster_tasks = list(zip(output_paths, preview_paths))
            if args.workers == 1 or len(raster_tasks) == 1:
                for task in raster_tasks:
                    rasterize_frame(task)
            else:
                with ThreadPoolExecutor(max_workers=args.workers) as executor:
                    list(executor.map(rasterize_frame, raster_tasks))
        if args.animation_webp is not None:
            frame_durations: int | Sequence[int]
            if args.frame_duration is not None:
                frame_durations = args.frame_duration
            else:
                frame_durations = frame_durations_for_rate(
                    frame_count,
                    swf_frame_rate(character_renderer),
                )
            animation_output = args.animation_webp.expanduser().resolve()
            use_parallel_webp = (
                len(preview_paths) > 1
                and args.webp_encoder != "pillow"
                and cwebp is not None
                and webpmux is not None
            )
            encoder_name = "parallel cwebp/webpmux" if use_parallel_webp else "Pillow"
            compression_name = (
                "exact-lossless"
                if args.webp_lossy_quality is None
                else f"lossy-Q{args.webp_lossy_quality:g}"
            )
            print(
                f"Encoding {len(preview_paths)}-frame {compression_name} WebP with "
                f"{encoder_name} (method={args.webp_method}"
                + (f", workers={args.workers}" if use_parallel_webp else "")
                + ")...",
                flush=True,
            )
            webp_started = perf_counter()
            if use_parallel_webp:
                assert cwebp is not None and webpmux is not None
                save_animation_webp_parallel(
                    preview_paths,
                    animation_output,
                    frame_durations=frame_durations,
                    workers=args.workers,
                    method=args.webp_method,
                    lossy_quality=args.webp_lossy_quality,
                    cwebp=cwebp,
                    webpmux=webpmux,
                )
            else:
                save_animation_webp_pillow(
                    preview_paths,
                    animation_output,
                    frame_durations=frame_durations,
                    method=args.webp_method,
                    lossy_quality=args.webp_lossy_quality,
                )
            webp_elapsed = perf_counter() - webp_started
            print(
                f"Encoded WebP in {webp_elapsed:.1f}s "
                f"({animation_output.stat().st_size / (1024 * 1024):.1f} MiB) "
                f"-> {animation_output}",
                flush=True,
            )
    finally:
        if temporary_context is not None:
            temporary_context.cleanup()

    print(
        f"Composed {fields.get('strName', args.username)} as {len(layers)} SVG layers "
        f"from {len(set(sources.values()))} asset SWFs"
        + (" plus 1 SVG override" if svg_override_path is not None else "")
        + (
            f" as {frame_count} nested frames "
            f"{args.subframe_start}-{args.subframe_start + frame_count - 1}"
            if frame_count > 1 or args.subframe_start > 1
            else ""
        )
        + (
            " (complete detected loop)"
            if detected_loop is not None and frame_count == detected_loop
            else ""
        )
        + f" -> {output_paths[0]}"
        + (f" ... {output_paths[-1]}" if len(output_paths) > 1 else "")
    )
    for warning in warnings:
        print(f"Warning: {warning}", file=sys.stderr)
    return output_paths[0]


def main() -> int:
    parser = build_parser()
    try:
        run(parser.parse_args())
    except (CharacterSvgError, tryon.TryOnError, OSError, subprocess.SubprocessError) as error:
        parser.exit(1, f"error: {error}\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
