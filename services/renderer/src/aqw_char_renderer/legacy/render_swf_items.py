#!/usr/bin/env python3
"""Render AQW item SWF assets into transparent images.

Item SWFs in ``swf_assets/items/<category>/*.swf`` are self-contained Flash
sprites. Most non-armor items expose one "root" sprite whose name matches the
SWF file stem (declared via a SymbolClass tag). Static runs ask FFDec for that
root's selected frame as SVG, find its visible bounds with a bounded alpha
probe, and rasterize the tight vector artwork at the requested output size.
Assets whose SVG state is incompatible automatically fall back to FFDec PNG.

Armor files (``classes/M|/F/*.swf``) are NOT rendered here: they hold many
separately-anchored body-part symbols (chest, thigh, hand, foot, shin, head,
...), so a full standing armor render requires the character renderer rather
than a single sprite. Those records are reported and skipped.

Outputs are written to a completely separate directory (``swf_assets`` is never
modified). Animation runs produce lossless RGBA PNG previews and WebPs; static
``--png-only`` runs produce one lossless RGBA PNG per asset.
A ``gallery.html`` is written so results can be inspected in a browser.

Examples:

    ./venv/bin/python pipeline/render_swf_items.py --limit 12
    ./venv/bin/python pipeline/render_swf_items.py --ids 1,2,3
    ./venv/bin/python pipeline/render_swf_items.py --slots weapon helm cape
    ./venv/bin/python pipeline/render_swf_items.py --limit 24 --scale 2
    ./venv/bin/python pipeline/render_swf_items.py --png-only --unique-swfs --max-size 512
    ./venv/bin/python pipeline/render_swf_items.py --png-only --game-scale --max-size 512
    ./venv/bin/python pipeline/render_swf_items.py --png-only --save-svg --png-canvas square --max-size 512
"""

from __future__ import annotations

import argparse
from concurrent.futures import ThreadPoolExecutor, as_completed
import html
import json
import math
import re
import shutil
import struct
import subprocess
import tempfile
import time
import xml.etree.ElementTree as ET
import zlib
from pathlib import Path
from typing import Sequence

import numpy as np
from PIL import Image, ImageOps

REPO_ROOT = Path(__file__).resolve().parents[5]
DEFAULT_DATABASE = REPO_ROOT / "bot" / "assets" / "swf_item_index" / "item_db.json"
DEFAULT_SWF_DIR = REPO_ROOT / "bot" / "assets" / "swf_item_index" / "swf_assets"
DEFAULT_OUTPUT_DIR = REPO_ROOT / "render_outputs" / "items"
DEFAULT_FFDEC = "/Applications/FFDec.app/Contents/Resources/ffdec-cli.jar"
DEFAULT_SVG_PROBE_SIZE = 1024
WEAPON_ROTATION_DEG = -106  # Pillow angle: clockwise 106 degrees
WEAPON_SLOTS = frozenset(
    {"weapon", "gauntlet", "handgun", "rifle", "whip", "unarmed"}
)
FFDEC_SVG_SUBSPRITE_FIX_VERSION = (24, 1, 2)

# Exact display-list transforms from AQW's official characterB.swf, exported
# with FFDec 26.2.1. SWF translations are stored in twips, so the XML values
# below have been divided by 20. The main character-detail timeline places
# AvatarMC at +/-2.2980957; only its magnitude is used for tight standalone
# item images because facing direction is a presentation choice.
CHARACTER_DETAIL_DISPLAY_SCALE = 2.2980957
CHARACTER_MC_TRANSFORM = (
    1.0010376,
    0.0,
    0.0,
    1.0003357,
    -9 / 20,
    -7 / 20,
)
CHARACTER_WEAPON_HOLDER_TRANSFORM = (
    0.05911255,
    -0.2086029,
    -0.20854187,
    -0.059127808,
    -206 / 20,
    -938 / 20,
)
CHARACTER_CAPE_HOLDER_TRANSFORM = (
    0.28709412,
    0.0066070557,
    0.031158447,
    0.3423462,
    -175 / 20,
    -1862 / 20,
)
CHARACTER_HEAD_TRANSFORM = (
    0.23699951,
    0.0,
    0.0,
    0.23699951,
    122 / 20,
    -2007 / 20,
)
CHARACTER_HELM_HOLDER_TRANSFORM = (
    1.000412,
    0.0,
    0.0,
    1.0,
    0.0,
    0.0,
)
CHARACTER_FRONT_HAND_TRANSFORM = (
    -0.25798035,
    0.05406189,
    -0.05406189,
    -0.2579956,
    -340 / 20,
    -1228 / 20,
)
# AvatarMC.loadWeapon() gives a gauntlet child 0.8 scale, then mirrors X.
CHARACTER_GAUNTLET_CHILD_TRANSFORM = (-0.8, 0.0, 0.0, 0.8, 0.0, 0.0)
CHARACTER_PET_TRANSFORM = (1.0, 0.0, 0.0, 1.0, -40.0, 10.0)
# AvatarMC.loadMisc() overwrites cShadow's authored scale with mcChar's scale.
CHARACTER_GROUND_TRANSFORM = (
    CHARACTER_MC_TRANSFORM[0],
    0.0,
    0.0,
    CHARACTER_MC_TRANSFORM[3],
    0.0,
    0.0,
)

SVG_NAMESPACE = "http://www.w3.org/2000/svg"
XLINK_NAMESPACE = "http://www.w3.org/1999/xlink"
FFDEC_NAMESPACE = "https://www.free-decompiler.com/flash"
ET.register_namespace("", SVG_NAMESPACE)
ET.register_namespace("xlink", XLINK_NAMESPACE)
ET.register_namespace("ffdec", FFDEC_NAMESPACE)


def parse_ffdec_version(output: str) -> tuple[int, int, int] | None:
    """Extract FFDec's semantic version from its CLI banner."""
    match = re.search(
        r"JPEXS Free Flash Decompiler v\.(\d+)\.(\d+)(?:\.(\d+))?",
        output,
    )
    if match is None:
        return None
    return tuple(int(part or 0) for part in match.groups())  # type: ignore[return-value]


def detect_ffdec_version(ffdec: str) -> tuple[int, int, int] | None:
    """Return the installed FFDec version, or ``None`` if it cannot be read."""
    try:
        result = subprocess.run(
            [
                "java",
                "-Djava.awt.headless=true",
                "-jar",
                ffdec,
                "-help",
            ],
            capture_output=True,
            text=True,
            timeout=30,
        )
    except (OSError, subprocess.SubprocessError):
        return None
    return parse_ffdec_version(f"{result.stdout}\n{result.stderr}")


def sanitize_filename(name: str) -> str:
    cleaned = re.sub(r'[^\w\- .()\u4e00-\u9fff]', "_", str(name))
    cleaned = re.sub(r"\s+", "_", cleaned).strip("._ ")
    return cleaned or "item"


def load_database(path: Path) -> list[dict]:
    with path.open("r", encoding="utf-8") as handle:
        payload = json.load(handle)
    if not isinstance(payload, list):
        raise ValueError(f"Expected a JSON list in {path}")
    return [r for r in payload if isinstance(r, dict)]


def relative_swf_path(record: dict) -> str:
    """Return the swf relocation path for a record, mirroring download script."""
    slot = str(record.get("slot") or "").strip().casefold()
    raw = str(record.get("file") or "").strip()
    raw = raw.replace("\\", "/").split("?")[0].split("#")[0].lstrip("/")
    if raw.casefold().startswith("gamefiles/"):
        raw = raw[len("gamefiles/") :]
    if slot == "armor" and "/" not in raw and raw:
        return raw  # resolve to classes/{M,F} below
    return raw


def resolve_local_paths(record: dict, swf_dir: Path) -> list[Path]:
    raw = relative_swf_path(record)
    if not raw:
        return []
    if str(record.get("slot") or "").strip().casefold() == "armor" and "/" not in raw:
        out = []
        for gender in ("M", "F"):
            p = swf_dir / "classes" / gender / raw
            if p.exists():
                out.append(p)
        return out
    p = swf_dir / raw
    return [p] if p.exists() else []


def add_unmapped_item_assets(
    file_groups: dict[Path, list[dict]], swf_dir: Path
) -> int:
    """Add local ``items/`` SWFs that have no record in the item database."""
    known = {
        str(path.relative_to(swf_dir)).casefold()
        for path in file_groups
    }
    added = 0
    items_dir = swf_dir / "items"
    for asset in sorted(items_dir.rglob("*")):
        if not asset.is_file() or asset.suffix.casefold() != ".swf":
            continue
        relative = asset.relative_to(swf_dir)
        key = str(relative).casefold()
        if key in known:
            continue
        category = relative.parts[1].casefold() if len(relative.parts) > 2 else ""
        if category == "capes":
            slot = "cape"
        elif category == "helms":
            slot = "helm"
        elif category == "pets":
            slot = "pet"
        elif category == "grounds":
            slot = "ground"
        elif category == "gauntlets":
            slot = "gauntlet"
        else:
            slot = "weapon"
        file_groups[asset] = [
            {
                "id": "asset",
                "name": "__".join(relative.with_suffix("").parts),
                "file": relative.as_posix(),
                "slot": slot,
                "unmapped_asset": True,
            }
        ]
        known.add(key)
        added += 1
    return added


def mirrored_asset_output_path(
    source: Path, swf_dir: Path, output_dir: Path, suffix: str = ".png"
) -> Path:
    """Mirror an asset's SWF-relative path under an output directory."""
    if not suffix.startswith("."):
        suffix = f".{suffix}"
    return output_dir / source.relative_to(swf_dir).with_suffix(suffix)


def default_svg_output_dir(output_dir: Path) -> Path:
    """Return a sibling output root so saved vectors never mix with PNGs."""
    return output_dir.with_name(f"{output_dir.name}_svg")


def item_output_stem(item: dict) -> str:
    return f"{item['id']}_{sanitize_filename(item['name'])}"


def png_output_path(
    source: Path,
    item: dict,
    swf_dir: Path,
    output_dir: Path,
    png_dir: Path,
    mirror_asset_tree: bool,
) -> Path:
    if mirror_asset_tree:
        return mirrored_asset_output_path(source, swf_dir, output_dir)
    return png_dir / f"{item_output_stem(item)}.png"


def select_items(database, args):
    items = list(database)
    if args.ids:
        id_set = {int(x) for x in args.ids.split(",") if x.strip()}
        items = [r for r in items if r.get("id") in id_set]
    elif args.slots:
        wanted = {s.casefold() for s in args.slots}
        items = [r for r in items if str(r.get("slot") or "").strip().casefold() in wanted]
    elif not args.no_skip_armor:
        items = [r for r in items if str(r.get("slot") or "").strip().casefold() != "armor"]
    if args.limit:
        items = items[: args.limit]
    return items


def _zoom_args(zoom: float) -> list[str]:
    return ["-zoom", str(zoom)] if zoom and zoom != 1 else []


def build_sprite_export_command(
    ffdec: str,
    zoom: float,
    batch_input: Path,
    batch_output: Path,
    selected_ids: Sequence[int] = (),
    selected_frames: dict[int, int] | None = None,
    output_format: str = "png",
) -> list[str]:
    if output_format not in {"png", "svg"}:
        raise ValueError(f"Unsupported sprite export format: {output_format}")
    cmd = ["java", "-Djava.awt.headless=true", "-jar", ffdec]
    cmd += _zoom_args(zoom)
    if selected_ids:
        cmd += ["-selectid", ",".join(str(sprite_id) for sprite_id in selected_ids)]
    if selected_frames:
        cmd += [
            "-select",
            ",".join(
                f"{sprite_id}:{frame}"
                for sprite_id, frame in selected_frames.items()
            ),
        ]
    cmd += ["-format", f"sprite:{output_format}", "-export", "sprite",
            str(batch_output), str(batch_input)]
    return cmd


def export_sprites(
    ffdec: str,
    zoom: float,
    batch_input: Path,
    batch_output: Path,
    selected_ids: Sequence[int] = (),
    selected_frames: dict[int, int] | None = None,
    output_format: str = "png",
) -> None:
    cmd = build_sprite_export_command(
        ffdec,
        zoom,
        batch_input,
        batch_output,
        selected_ids,
        selected_frames,
        output_format,
    )
    subprocess.run(cmd, check=True, capture_output=True)


def parse_swf_sprite_metadata(data: bytes, symbol_name: str) -> dict:
    """Read root-sprite metadata directly from an unencrypted FWS/CWS file.

    This avoids invoking FFDec merely to discover a character ID. ZWS/LZMA or
    malformed files return an empty mapping and retain the full-export fallback.
    """
    if len(data) < 9 or data[:3] not in {b"FWS", b"CWS"}:
        return {}
    try:
        body = data[8:] if data[:3] == b"FWS" else zlib.decompress(data[8:])
    except zlib.error:
        return {}
    if not body:
        return {}

    rect_bits = 5 + 4 * (body[0] >> 3)
    position = (rect_bits + 7) // 8 + 4  # RECT + frame rate + frame count
    sprite_frames: dict[int, int] = {}
    sprite_payloads: dict[int, bytes] = {}
    symbols: list[tuple[int, str]] = []

    while position + 2 <= len(body):
        tag_header = struct.unpack_from("<H", body, position)[0]
        position += 2
        tag_code = tag_header >> 6
        tag_length = tag_header & 0x3F
        if tag_length == 0x3F:
            if position + 4 > len(body):
                break
            tag_length = struct.unpack_from("<I", body, position)[0]
            position += 4
        tag_end = position + tag_length
        if tag_end > len(body):
            break
        payload = body[position:tag_end]
        position = tag_end

        if tag_code == 0:  # End
            break
        if tag_code == 39 and len(payload) >= 4:  # DefineSprite
            sprite_id, frame_count = struct.unpack_from("<HH", payload, 0)
            sprite_frames[sprite_id] = frame_count
            sprite_payloads[sprite_id] = payload
        elif tag_code == 76 and len(payload) >= 2:  # SymbolClass
            count = struct.unpack_from("<H", payload, 0)[0]
            offset = 2
            for _ in range(count):
                if offset + 2 > len(payload):
                    break
                sprite_id = struct.unpack_from("<H", payload, offset)[0]
                offset += 2
                terminator = payload.find(b"\0", offset)
                if terminator < 0:
                    break
                name = payload[offset:terminator].decode("utf-8", errors="replace")
                symbols.append((sprite_id, name))
                offset = terminator + 1

    if not sprite_frames:
        return {}
    wanted = symbol_name.casefold()
    root_id = next(
        (
            sprite_id
            for sprite_id, name in symbols
            if name.casefold() == wanted
            or name.casefold().rsplit(".", 1)[-1] == wanted
        ),
        None,
    )
    if root_id is None and len(symbols) == 1:
        root_id = symbols[0][0]
    if root_id not in sprite_frames:
        root_id = max(sprite_frames)
    metadata = {
        "root": root_id,
        "root_frame_count": sprite_frames[root_id],
        "sprite_count": len(sprite_frames),
        "total_sprite_frames": sum(sprite_frames.values()),
    }
    root_payload = sprite_payloads.get(root_id, b"")
    current_frame = 0
    position = 4  # sprite id + declared frame count
    while position + 2 <= len(root_payload):
        tag_header = struct.unpack_from("<H", root_payload, position)[0]
        position += 2
        tag_code = tag_header >> 6
        tag_length = tag_header & 0x3F
        if tag_length == 0x3F:
            if position + 4 > len(root_payload):
                break
            tag_length = struct.unpack_from("<I", root_payload, position)[0]
            position += 4
        tag_end = position + tag_length
        if tag_end > len(root_payload):
            break
        payload = root_payload[position:tag_end]
        position = tag_end
        if tag_code == 0:
            break
        if tag_code == 1:  # ShowFrame
            current_frame += 1
        elif tag_code == 43:  # FrameLabel
            terminator = payload.find(b"\0")
            if terminator >= 0:
                label = payload[:terminator].decode("utf-8", errors="replace")
                normalized = label.casefold()
                if normalized in {"idle", "idel", "id"}:
                    metadata.setdefault("idle_frame", current_frame + 1)
                elif normalized == "ready":
                    metadata.setdefault("ready_frame", current_frame + 1)
    return metadata


def read_swf_sprite_metadata(swf: Path) -> dict:
    try:
        return parse_swf_sprite_metadata(swf.read_bytes(), swf.stem)
    except OSError:
        return {}


def select_static_root_frame(metadata: dict) -> int:
    """Choose one deterministic display frame from authored root labels.

    When Ready leads into Idle, the final Ready frame is the authored pose
    immediately before the idle loop begins. Assets without that transition
    fall back to their first Idle, first Ready, or first root frame.
    """
    ready = metadata.get("ready_frame")
    idle = metadata.get("idle_frame")
    if (
        isinstance(ready, int)
        and isinstance(idle, int)
        and 1 <= ready < idle
    ):
        return idle - 1
    if isinstance(idle, int) and idle >= 1:
        return idle
    if isinstance(ready, int) and ready >= 1:
        return ready
    return 1


def svg_static_state_is_compatible(
    metadata: dict,
    ffdec_version: tuple[int, int, int] | None = None,
) -> bool:
    """Whether FFDec SVG preserves the selected root/nested timeline state.

    Root frame 1 is safe on every supported FFDec version. FFDec versions
    before 24.1.2 could leave nested clips at their initial state when a later
    root frame was selected (upstream issue #2589), so retain the PNG fallback
    when an old or unknown version is used.
    """
    return (
        select_static_root_frame(metadata) == 1
        or ffdec_version is not None
        and ffdec_version >= FFDEC_SVG_SUBSPRITE_FIX_VERSION
    )


def build_ffdec_animation_command(
    ffdec: str,
    zoom: float,
    swf: Path,
    root_id: int,
    root_frame: int,
    out_dir: Path,
    sublength: int,
) -> list[str]:
    """Build the FFDec command for a frozen root with advancing children."""
    if root_id < 1 or root_frame < 1 or sublength < 1:
        raise ValueError("root_id, root_frame, and sublength must be positive")
    return [
        "java",
        "-Djava.awt.headless=true",
        "-jar",
        ffdec,
        *_zoom_args(zoom),
        "-selectid",
        str(root_id),
        "-select",
        f"{root_id}:{root_frame}",
        "-sublength",
        str(sublength),
        "-format",
        "sprite:png",
        "-export",
        "sprite",
        str(out_dir),
        str(swf),
    ]


def export_idle_animated(
    ffdec: str,
    zoom: float,
    swf: Path,
    root_id: int,
    idle_frame: int,
    out_dir: Path,
    sublength: int = 240,
) -> list[Path] | None:
    """Export the Idle root frame with `-sublength` so nested sub-sprites animate.

    This reproduces AS-driven pets where the root freezes on its Idle frame and
    nested tail/ear/eye sprites loop internally. Returns the sorted list of
    exported PNG subframes (1-indexed), or None on failure.
    """
    out_dir.mkdir(parents=True, exist_ok=True)
    cmd = build_ffdec_animation_command(
        ffdec, zoom, swf, root_id, idle_frame, out_dir, sublength
    )
    try:
        subprocess.run(
            cmd,
            check=True,
            capture_output=True,
            timeout=max(180, sublength * 2),
        )
    except Exception:
        return None
    # Locate exported frames under <out>/DefineSprite_<root>_* / <idle_frame>/
    found = []
    for sprite_dir in out_dir.glob(f"DefineSprite_{root_id}*"):
        for frame_dir in sprite_dir.glob(f"{idle_frame}"):
            found.extend(sorted(frame_dir.glob("*.png"), key=lambda p: int(p.stem)))
    return found or None


def select_subframes(
    paths: Sequence[Path], start: int = 1, count: int | None = None
) -> list[Path]:
    """Select a 1-indexed, consecutive window from FFDec subframes."""
    if start < 1:
        raise ValueError("subframe start must be at least 1")
    if count is not None and count < 1:
        raise ValueError("subframe count must be at least 1")
    ordered = sorted(paths, key=lambda path: int(path.stem))
    selected = ordered[start - 1 :] if count is None else ordered[start - 1 : start - 1 + count]
    if count is not None and len(selected) != count:
        raise ValueError(
            f"Requested {count} subframes starting at {start}, but only "
            f"{len(selected)} were available"
        )
    return selected


def named_sprites(file_output: Path) -> list[Path]:
    named = []
    for d in file_output.iterdir():
        if not d.is_dir():
            continue
        name = d.name
        if "_fla." in name.casefold():
            continue
        if re.fullmatch(r"DefineSprite_\d+", name):
            continue
        named.append(d)
    return named


def exported_sprites(file_output: Path) -> list[Path]:
    """Return every FFDec sprite directory except internal FLA scaffolding."""
    return [
        directory
        for directory in file_output.iterdir()
        if directory.is_dir() and "_fla." not in directory.name.casefold()
    ]


def sprite_character_id(directory: Path) -> int:
    match = re.match(r"DefineSprite_(\d+)", directory.name)
    return int(match.group(1)) if match else -1


def discover_root_sprite(file_output: Path, stem: str) -> Path | None:
    named = named_sprites(file_output)
    if not named:
        return None
    stem_l = stem.casefold()

    def suffix(d):
        return re.sub(r"^DefineSprite_\d+_", "", d.name)

    for d in named:
        if suffix(d).casefold() == stem_l:
            return d
    for d in named:
        s = suffix(d).casefold()
        if s.startswith(stem_l + ".") or stem_l.startswith(s + "."):
            return d
    return None


def frame_content(png: Path) -> int:
    try:
        arr = np.array(Image.open(png).convert("RGBA"))
        return int((arr[:, :, 3] > 8).sum())
    except Exception:
        return 0


def visible_alpha(img: Image.Image) -> int:
    bb = img.getbbox()
    return 0 if bb is None else (bb[2] - bb[0]) * (bb[3] - bb[1])


def root_by_content(file_output: Path) -> Path | None:
    """Choose the compiled root when no SymbolClass name is available.

    Older AQW assets often have only generic ``DefineSprite_N`` names. Their
    composed root sprite is emitted last and has the highest character id; its
    lower-id dependencies are partial handles, blades, effects, and so on.
    Selecting by opaque-pixel count is incorrect for long, narrow weapons
    because a large blade component can contain more pixels than the complete
    assembled item.
    """
    sprites = exported_sprites(file_output)
    if not sprites:
        return None
    return max(sprites, key=lambda directory: (sprite_character_id(directory), directory.name))


def find_exported_root(
    file_output: Path, stem: str, selected_root: int | None = None
) -> Path | None:
    """Find the composed root directory in one FFDec file export."""
    if not file_output.is_dir():
        return None
    if selected_root is not None:
        selected = next(
            (
                sprite
                for sprite in exported_sprites(file_output)
                if sprite_character_id(sprite) == selected_root
            ),
            None,
        )
        if selected is not None:
            return selected
    return discover_root_sprite(file_output, stem) or root_by_content(file_output)


def union_bbox(imgs: list[Image.Image]) -> tuple[int, int, int, int]:
    x0 = min(b[0] for b in (i.getbbox() for i in imgs) if b)
    y0 = min(b[1] for b in (i.getbbox() for i in imgs) if b)
    x1 = max(b[2] for b in (i.getbbox() for i in imgs) if b)
    y1 = max(b[3] for b in (i.getbbox() for i in imgs) if b)
    return x0, y0, x1, y1


def parse_idle_info_from_xml(xml: str, symbol_name: str) -> dict:
    """Parse FFDec XML into root/Idle metadata without invoking FFDec.

    Frame labels are zero-positioned relative to the number of preceding
    ``ShowFrame`` tags. FFDec's ``-select`` syntax is one-indexed, so ``first``
    and ``last`` in the returned mapping are one-indexed.
    """
    try:
        document = ET.fromstring(xml)
    except ET.ParseError:
        return {"animated": False}

    frame_rate = float(document.attrib.get("frameRate", 0) or 0)
    symbol_ids: list[int] = []
    symbol_names: list[str] = []
    for item in document.iter("item"):
        if item.attrib.get("type") != "SymbolClassTag":
            continue
        tags = item.find("tags")
        names = item.find("names")
        if tags is not None:
            symbol_ids.extend(
                int(child.text)
                for child in tags.findall("item")
                if child.text and child.text.strip().isdigit()
            )
        if names is not None:
            symbol_names.extend(
                (child.text or "").strip() for child in names.findall("item")
            )

    root_id = next(
        (
            sprite_id
            for sprite_id, name in zip(symbol_ids, symbol_names)
            if name.casefold() == symbol_name.casefold()
        ),
        None,
    )
    if root_id is None and len(symbol_ids) == 1:
        root_id = symbol_ids[0]
    if root_id is None:
        return {"animated": False, "frame_rate": frame_rate}

    sprite = next(
        (
            item
            for item in document.iter("item")
            if item.attrib.get("type") == "DefineSpriteTag"
            and item.attrib.get("spriteId") == str(root_id)
        ),
        None,
    )
    sub_tags = sprite.find("subTags") if sprite is not None else None
    if sub_tags is None:
        return {"animated": False, "root": root_id, "frame_rate": frame_rate}

    current_frame = 0
    labels: list[tuple[int, str]] = []
    for item in sub_tags.findall("item"):
        item_type = item.attrib.get("type")
        if item_type == "ShowFrameTag":
            current_frame += 1
        elif item_type == "FrameLabelTag":
            labels.append((current_frame, item.attrib.get("name", "")))

    idle_position = next(
        (
            index
            for index, (_, name) in enumerate(labels)
            if name.casefold() in {"idle", "idel", "id"}
        ),
        None,
    )
    if idle_position is None:
        return {"animated": False, "root": root_id, "frame_rate": frame_rate}

    idle_zero = labels[idle_position][0]
    next_zero = (
        labels[idle_position + 1][0]
        if idle_position + 1 < len(labels)
        else current_frame
    )
    # The labeled frame is included. The next labeled frame is not.
    last_one = max(idle_zero + 1, next_zero)
    first_one = idle_zero + 1
    return {
        "animated": True,
        "root": root_id,
        "first": first_one,
        "last": last_one,
        "frame_rate": frame_rate,
        "idle_csv": f"{root_id}:{first_one}",
    }


def parse_idle_info(swf: Path, ffdec: str) -> dict:
    """Export FFDec XML to a temporary directory and parse Idle metadata."""
    try:
        with tempfile.TemporaryDirectory(prefix="ffdec_xml_") as temp_dir:
            xml_path = Path(temp_dir) / f"{swf.stem}.xml"
            subprocess.run(
                [
                    "java",
                    "-Djava.awt.headless=true",
                    "-jar",
                    ffdec,
                    "-swf2xml",
                    str(swf),
                    str(xml_path),
                ],
                check=True,
                capture_output=True,
                text=True,
                timeout=120,
            )
            xml = xml_path.read_text(encoding="utf-8", errors="replace")
    except (OSError, subprocess.SubprocessError):
        return {"animated": False}
    return parse_idle_info_from_xml(xml, swf.stem)


def parse_idle_frame_span(swf: Path, ffdec: str) -> tuple[int, int, int] | None:
    """Backward-compatible tuple form of :func:`parse_idle_info`."""
    info = parse_idle_info(swf, ffdec)
    if not info.get("animated"):
        return None
    return info["first"], info["last"], info["root"]


def resize_by_scale(img: Image.Image, scale: float) -> Image.Image:
    """Resize an image by any positive multiplier, preserving aspect ratio."""
    if scale <= 0:
        raise ValueError("scale must be positive")
    if math.isclose(scale, 1.0):
        return img
    target = tuple(
        max(1, round(dimension * scale))
        for dimension in img.size
    )
    return img.resize(target, Image.Resampling.LANCZOS)


def align_images(imgs: list[Image.Image], scale: float) -> list[Image.Image] | None:
    """Crop a list of RGBA frames to a shared bbox and resize."""
    imgs = [i.convert("RGBA") for i in imgs if i is not None]
    if not imgs:
        return None
    x0, y0, x1, y1 = union_bbox(imgs)
    imgs = [i.crop((x0, y0, x1, y1)) for i in imgs]
    if not math.isclose(scale, 1.0):
        imgs = [resize_by_scale(img, scale) for img in imgs]
    return imgs


def place_frames_on_canvas(
    frames: Sequence[Image.Image],
    size: tuple[int, int],
    offset: tuple[int, int] = (0, 0),
) -> list[Image.Image]:
    """Place aligned frames on a fixed transparent canvas, clipping as needed."""
    width, height = size
    if width < 1 or height < 1:
        raise ValueError("canvas dimensions must be positive")
    placed: list[Image.Image] = []
    for frame in frames:
        canvas = Image.new("RGBA", size, (0, 0, 0, 0))
        canvas.alpha_composite(frame.convert("RGBA"), offset)
        placed.append(canvas)
    return placed


def build_frame_durations(
    frame_count: int,
    default_ms: int,
    leading_ms: Sequence[int] = (),
) -> list[int]:
    """Return per-frame WebP durations with optional leading overrides."""
    if frame_count < 1:
        raise ValueError("frame_count must be positive")
    if default_ms < 1 or any(value < 1 for value in leading_ms):
        raise ValueError("frame durations must be positive")
    if len(leading_ms) > frame_count:
        raise ValueError("more leading durations were supplied than frames")
    return [*leading_ms, *([default_ms] * (frame_count - len(leading_ms)))]


def parse_positive_int_csv(value: str) -> list[int]:
    """Parse a comma-separated list of positive integers."""
    if not value.strip():
        return []
    try:
        parsed = [int(part.strip()) for part in value.split(",")]
    except ValueError as exc:
        raise argparse.ArgumentTypeError("expected comma-separated integers") from exc
    if any(number < 1 for number in parsed):
        raise argparse.ArgumentTypeError("values must be positive")
    return parsed


def save_lossless_webp(
    frames: Sequence[Image.Image],
    output: Path,
    durations_ms: int | Sequence[int],
) -> None:
    """Save one or more RGBA frames as a lossless looping WebP."""
    if not frames:
        raise ValueError("at least one frame is required")
    output.parent.mkdir(parents=True, exist_ok=True)
    if len(frames) == 1:
        frames[0].save(output, format="WEBP", lossless=True, method=6)
        return
    frames[0].save(
        output,
        save_all=True,
        append_images=list(frames[1:]),
        duration=durations_ms,
        loop=0,
        lossless=True,
        method=6,
        format="WEBP",
    )


def render_frames(
    sprite_dir: Path,
    scale: float,
    idle_span: tuple[int, int] | None = None,
) -> list[Image.Image] | None:
    """Render the idle animation frames of a sprite.

    When ``idle_span`` (1-indexed PNG range) is given, only those frames are
    used, which isolates the Idle pose and excludes Walk/Attack segments.
    Otherwise all non-empty frames are used. Empty frames are skipped.
    Frames are cropped to an identical bounding box so the loop is stable.
    """
    frames = sorted(sprite_dir.glob("*.png"), key=lambda p: int(p.stem))
    imgs: list[Image.Image] = []
    for frame in frames:
        n = int(frame.stem)
        if idle_span is not None and not (idle_span[0] <= n <= idle_span[1]):
            continue
        try:
            img = Image.open(frame).convert("RGBA")
        except Exception:
            continue
        if visible_alpha(img) == 0:
            continue
        imgs.append(img)
    if not imgs:
        return None
    x0, y0, x1, y1 = union_bbox(imgs)
    imgs = [i.crop((x0, y0, x1, y1)) for i in imgs]
    if not math.isclose(scale, 1.0):
        imgs = [resize_by_scale(img, scale) for img in imgs]
    return imgs


def render_best_frame(sprite_dir: Path, scale: float) -> Image.Image | None:
    """Return the first non-empty frame, cropped independently."""
    return render_static_frame(sprite_dir, scale)


def render_static_frame(
    sprite_dir: Path,
    scale: float,
    frame_number: int | None = None,
) -> Image.Image | None:
    """Render one preferred/first frame without unioning other frame bounds."""
    frames = sorted(sprite_dir.glob("*.png"), key=lambda path: int(path.stem))
    if frame_number is not None:
        preferred = [path for path in frames if int(path.stem) == frame_number]
        frames = preferred or frames
    for frame in frames:
        try:
            img = Image.open(frame).convert("RGBA")
        except Exception:
            continue
        bbox = img.getbbox()
        if bbox is None:
            continue
        img = img.crop(bbox)
        if not math.isclose(scale, 1.0):
            img = resize_by_scale(img, scale)
        return img
    return None


_SVG_LENGTH_RE = re.compile(
    r"^\s*([+-]?(?:\d+(?:\.\d*)?|\.\d+)(?:[eE][+-]?\d+)?)\s*(?:px)?\s*$"
)


def parse_svg_length(value: str | None) -> float | None:
    """Parse the unitless/px dimensions emitted by FFDec's SVG exporter."""
    if not value:
        return None
    match = _SVG_LENGTH_RE.fullmatch(value)
    if not match:
        return None
    number = float(match.group(1))
    return number if math.isfinite(number) and number > 0 else None


def svg_canvas_viewbox(svg_path: Path) -> tuple[float, float, float, float] | None:
    """Return an SVG's user-space viewport as ``x, y, width, height``."""
    try:
        root = ET.parse(svg_path).getroot()
    except (ET.ParseError, OSError):
        return None
    raw_viewbox = root.attrib.get("viewBox")
    if raw_viewbox:
        try:
            values = [float(value) for value in re.split(r"[\s,]+", raw_viewbox.strip())]
        except ValueError:
            values = []
        if (
            len(values) == 4
            and all(math.isfinite(value) for value in values)
            and values[2] > 0
            and values[3] > 0
        ):
            return tuple(values)  # type: ignore[return-value]
    width = parse_svg_length(root.attrib.get("width"))
    height = parse_svg_length(root.attrib.get("height"))
    if width is None or height is None:
        return None
    return 0.0, 0.0, width, height


def alpha_bbox(img: Image.Image) -> tuple[int, int, int, int] | None:
    """Return visible RGBA bounds without counting RGB hidden under alpha 0."""
    return img.convert("RGBA").getchannel("A").getbbox()


def _render_svg_with_resvg(
    svg_path: Path,
    output_path: Path,
    maximum: int,
    resvg: str,
) -> tuple[int, int] | None:
    """Rasterize with resvg (single static binary, much faster than librsvg).

    Passes the dominant dimension to resvg exactly like the rsvg-convert
    invocation. resvg fits the full viewBox into that bound, so no content is
    clipped; the only difference from rsvg-convert is a possible 1px rounding
    of the derived dimension, which is consistent across every frame of a job
    (the shared viewbox is fixed), so delta-cropping remains aligned.
    """
    viewbox = svg_canvas_viewbox(svg_path)
    if viewbox is None or maximum < 1:
        return None
    _, _, width, height = viewbox
    if width <= 0 or height <= 0:
        return None
    size_flag = "--width" if width >= height else "--height"
    command = [resvg, size_flag, str(maximum), str(svg_path), str(output_path)]
    try:
        subprocess.run(command, check=True, capture_output=True, timeout=120)
        with Image.open(output_path) as rendered:
            rendered.load()
            return rendered.size
    except (OSError, subprocess.SubprocessError, ValueError):
        return None


def render_svg_to_maximum(
    svg_path: Path,
    output_path: Path,
    maximum: int,
    rsvg_convert: str,
    square_canvas_size: int | None = None,
) -> tuple[int, int] | None:
    """Rasterize an SVG with its longest artwork dimension at ``maximum``.

    When ``square_canvas_size`` is supplied, the tightly framed SVG is placed
    directly on a centered transparent square output page. This changes only
    the PNG framing; callers can still retain the original tight SVG.

    Dispatches to resvg when the configured binary is resvg.
    """
    if Path(rsvg_convert).name.startswith("resvg"):
        return _render_svg_with_resvg(svg_path, output_path, maximum, rsvg_convert)
    viewbox = svg_canvas_viewbox(svg_path)
    if viewbox is None or maximum < 1:
        return None
    _, _, width, height = viewbox
    size_option = "--width" if width >= height else "--height"
    command = [
        rsvg_convert,
        "--format",
        "png",
        size_option,
        str(maximum),
    ]
    if square_canvas_size is not None:
        canvas_size = max(maximum, square_canvas_size)
        content_scale = maximum / max(width, height)
        rendered_width = width * content_scale
        rendered_height = height * content_scale
        command.extend(
            [
                "--page-width",
                str(canvas_size),
                "--page-height",
                str(canvas_size),
                "--left",
                f"{(canvas_size - rendered_width) / 2:.8g}",
                "--top",
                f"{(canvas_size - rendered_height) / 2:.8g}",
            ]
        )
    command.extend(["--output", str(output_path), str(svg_path)])
    try:
        subprocess.run(
            command,
            check=True,
            capture_output=True,
            timeout=120,
        )
        with Image.open(output_path) as rendered:
            rendered.load()
            return rendered.size
    except (OSError, subprocess.SubprocessError, ValueError):
        return None


def probe_svg_alpha(
    svg_path: Path,
    rsvg_convert: str,
    probe_size: int = DEFAULT_SVG_PROBE_SIZE,
) -> tuple[tuple[float, float, float, float], np.ndarray] | None:
    """Rasterize a bounded alpha-only geometry probe for an SVG."""
    canvas = svg_canvas_viewbox(svg_path)
    if canvas is None:
        return None
    probe_path = svg_path.with_name(f".{svg_path.stem}.bounds.png")
    if render_svg_to_maximum(svg_path, probe_path, probe_size, rsvg_convert) is None:
        probe_path.unlink(missing_ok=True)
        return None
    try:
        with Image.open(probe_path) as probe:
            probe = probe.convert("RGBA")
            alpha = np.asarray(probe.getchannel("A"), dtype=np.uint8).copy()
    except (OSError, ValueError):
        return None
    finally:
        probe_path.unlink(missing_ok=True)
    if alpha.size == 0 or not np.any(alpha):
        return None
    return canvas, alpha


def visible_viewbox_from_probe(
    probe: tuple[tuple[float, float, float, float], np.ndarray],
    padding_pixels: int = 0,
) -> tuple[float, float, float, float] | None:
    """Map a probe's visible alpha rectangle back into SVG user space."""
    canvas, alpha = probe
    occupied_rows = np.flatnonzero(np.any(alpha, axis=1))
    occupied_columns = np.flatnonzero(np.any(alpha, axis=0))
    if not len(occupied_rows) or not len(occupied_columns):
        return None
    probe_height, probe_width = alpha.shape
    bbox = (
        int(occupied_columns[0]),
        int(occupied_rows[0]),
        int(occupied_columns[-1]) + 1,
        int(occupied_rows[-1]) + 1,
    )

    canvas_x, canvas_y, canvas_width, canvas_height = canvas
    scale_x = canvas_width / probe_width
    scale_y = canvas_height / probe_height
    x0 = max(canvas_x, canvas_x + (bbox[0] - padding_pixels) * scale_x)
    y0 = max(canvas_y, canvas_y + (bbox[1] - padding_pixels) * scale_y)
    x1 = min(
        canvas_x + canvas_width,
        canvas_x + (bbox[2] + padding_pixels) * scale_x,
    )
    y1 = min(
        canvas_y + canvas_height,
        canvas_y + (bbox[3] + padding_pixels) * scale_y,
    )
    if x1 <= x0 or y1 <= y0:
        return None
    return x0, y0, x1 - x0, y1 - y0


def detect_svg_visible_viewbox(
    svg_path: Path,
    rsvg_convert: str,
    probe_size: int = DEFAULT_SVG_PROBE_SIZE,
    padding_pixels: int = 0,
) -> tuple[float, float, float, float] | None:
    """Find visible vector bounds using a bounded, transparent alpha probe."""
    probe = probe_svg_alpha(svg_path, rsvg_convert, probe_size)
    return (
        None
        if probe is None
        else visible_viewbox_from_probe(probe, padding_pixels)
    )


def is_weapon_slot(slot: str | None) -> bool:
    """Whether a database slot uses AQW's held-weapon orientation."""
    return str(slot or "").strip().casefold() in WEAPON_SLOTS


def transform_point(
    matrix: tuple[float, float, float, float, float, float],
    x: float,
    y: float,
) -> tuple[float, float]:
    """Apply an SVG affine matrix ``a b c d e f`` to one point."""
    a, b, c, d, e, f = matrix
    return a * x + c * y + e, b * x + d * y + f


def compose_transforms(
    outer: tuple[float, float, float, float, float, float],
    inner: tuple[float, float, float, float, float, float],
) -> tuple[float, float, float, float, float, float]:
    """Compose two SVG/SWF affine transforms as ``outer(inner(point))``."""
    oa, ob, oc, od, oe, of = outer
    ia, ib, ic, id_, ie, iff = inner
    return (
        oa * ia + oc * ib,
        ob * ia + od * ib,
        oa * ic + oc * id_,
        ob * ic + od * id_,
        oa * ie + oc * iff + oe,
        ob * ie + od * iff + of,
    )


def character_item_transform(
    slot: str | None,
) -> tuple[float, float, float, float, float, float] | None:
    """Return an item's exact characterB idle display-list transform.

    The result includes the slot holder and the near-identity ``mcChar``
    transform, but not the outer whole-avatar scale/mirror. Tight standalone
    crops intentionally discard placement while retaining exact scale,
    rotation, shear, and reflection.
    """
    normalized = str(slot or "").strip().casefold()
    if normalized in {"weapon", "handgun", "rifle", "whip", "unarmed"}:
        return compose_transforms(
            CHARACTER_MC_TRANSFORM,
            CHARACTER_WEAPON_HOLDER_TRANSFORM,
        )
    if normalized == "cape":
        return compose_transforms(
            CHARACTER_MC_TRANSFORM,
            CHARACTER_CAPE_HOLDER_TRANSFORM,
        )
    if normalized == "helm":
        return compose_transforms(
            compose_transforms(
                CHARACTER_MC_TRANSFORM,
                CHARACTER_HEAD_TRANSFORM,
            ),
            CHARACTER_HELM_HOLDER_TRANSFORM,
        )
    if normalized == "gauntlet":
        return compose_transforms(
            compose_transforms(
                CHARACTER_MC_TRANSFORM,
                CHARACTER_FRONT_HAND_TRANSFORM,
            ),
            CHARACTER_GAUNTLET_CHILD_TRANSFORM,
        )
    if normalized == "pet":
        return CHARACTER_PET_TRANSFORM
    if normalized == "ground":
        return CHARACTER_GROUND_TRANSFORM
    return None


def affine_geometric_scale(
    matrix: tuple[float, float, float, float, float, float],
) -> float:
    """Return ``sqrt(abs(determinant))`` for an SVG affine transform."""
    a, b, c, d, _, _ = matrix
    return math.sqrt(abs(a * d - b * c))


def game_scale_ffdec_zoom(
    slot: str | None,
    weapon_zoom: float = 3.0,
) -> float:
    """Derive a slot's FFDec SVG zoom from the accepted weapon baseline.

    FFDec inversely adjusts ordinary SVG stroke widths as export zoom rises.
    Matching ``slot scale / zoom`` to the weapon ratio gives consistent line
    weight after the character slot transform is applied.
    """
    if weapon_zoom <= 0:
        raise ValueError("weapon zoom must be positive")
    matrix = character_item_transform(slot)
    weapon_matrix = character_item_transform("weapon")
    if matrix is None or weapon_matrix is None:
        return 1.0
    return (
        weapon_zoom
        * affine_geometric_scale(matrix)
        / affine_geometric_scale(weapon_matrix)
    )


def single_record_slot(records: Sequence[dict]) -> str | None:
    """Return one normalized slot when every record agrees, otherwise None."""
    slots = {
        str(item.get("slot") or "").strip().casefold()
        for item in records
    }
    return next(iter(slots)) if len(slots) == 1 else None


def transformed_viewbox_from_probe(
    probe: tuple[tuple[float, float, float, float], np.ndarray],
    matrix: tuple[float, float, float, float, float, float],
) -> tuple[float, float, float, float] | None:
    """Transform actual visible probe pixels into a tight SVG-space bound."""
    canvas, alpha = probe
    visible = alpha > 0
    occupied_rows = np.flatnonzero(np.any(visible, axis=1))
    if not len(occupied_rows):
        return None
    canvas_x, canvas_y, canvas_width, canvas_height = canvas
    probe_height, probe_width = alpha.shape
    scale_x = canvas_width / probe_width
    scale_y = canvas_height / probe_height
    visible_rows = visible[occupied_rows]
    first_x = np.argmax(visible_rows, axis=1).astype(np.float64)
    last_x = (
        probe_width
        - 1
        - np.argmax(visible_rows[:, ::-1], axis=1)
    ).astype(np.float64)
    x0 = canvas_x + first_x * scale_x
    x1 = canvas_x + (last_x + 1) * scale_x
    y0 = canvas_y + occupied_rows.astype(np.float64) * scale_y
    y1 = y0 + scale_y
    a, b, c, d, e, f = matrix

    def extrema(
        x_coefficient: float,
        y_coefficient: float,
        offset: float,
    ) -> tuple[float, float]:
        lower = (
            x_coefficient * (x0 if x_coefficient >= 0 else x1)
            + y_coefficient * (y0 if y_coefficient >= 0 else y1)
            + offset
        )
        upper = (
            x_coefficient * (x1 if x_coefficient >= 0 else x0)
            + y_coefficient * (y1 if y_coefficient >= 0 else y0)
            + offset
        )
        return float(lower.min()), float(upper.max())

    min_x, max_x = extrema(a, c, e)
    min_y, max_y = extrema(b, d, f)
    if max_x <= min_x or max_y <= min_y:
        return None
    return min_x, min_y, max_x - min_x, max_y - min_y


def weapon_svg_transform(
    viewbox: tuple[float, float, float, float],
) -> tuple[
    tuple[float, float, float, float, float, float],
    tuple[float, float, float, float],
]:
    """Return the held-weapon SVG transform and its transformed bounds.

    Pillow uses positive angles counter-clockwise, whereas SVG's y-down user
    space makes a positive angle appear clockwise. Thus Pillow's -106 degree
    weapon rotation maps to a positive 106 degree SVG rotation. Mirroring and
    rotation both occur around the tight artwork's center.
    """
    x, y, width, height = viewbox
    center_x = x + width / 2
    center_y = y + height / 2
    radians = math.radians(-WEAPON_ROTATION_DEG)
    cosine = math.cos(radians)
    sine = math.sin(radians)

    # Horizontal mirror followed by clockwise rotation in y-down SVG space.
    a = -cosine
    b = -sine
    c = -sine
    d = cosine
    e = center_x - a * center_x - c * center_y
    f = center_y - b * center_x - d * center_y
    matrix = a, b, c, d, e, f

    corners = [
        transform_point(matrix, corner_x, corner_y)
        for corner_x, corner_y in (
            (x, y),
            (x + width, y),
            (x, y + height),
            (x + width, y + height),
        )
    ]
    min_x = min(point[0] for point in corners)
    min_y = min(point[1] for point in corners)
    max_x = max(point[0] for point in corners)
    max_y = max(point[1] for point in corners)
    return matrix, (min_x, min_y, max_x - min_x, max_y - min_y)


def write_svg_viewbox(
    source: Path,
    output: Path,
    viewbox: tuple[float, float, float, float],
    transform: tuple[float, float, float, float, float, float] | None = None,
    intrinsic_scale: float = 1.0,
) -> bool:
    """Write a cropped SVG, optionally transforming its rendered children."""
    try:
        tree = ET.parse(source)
        root = tree.getroot()
        if transform is not None:
            wrapper = ET.Element(
                f"{{{SVG_NAMESPACE}}}g",
                {
                    "transform": "matrix("
                    + " ".join(f"{value:.12g}" for value in transform)
                    + ")"
                },
            )
            definition_tags = {
                "defs",
                "style",
                "title",
                "desc",
                "metadata",
                "clipPath",
                "mask",
                "linearGradient",
                "radialGradient",
                "pattern",
                "filter",
                "symbol",
                "marker",
                "script",
            }
            rendered_children = [
                child
                for child in list(root)
                if child.tag.rsplit("}", 1)[-1] not in definition_tags
            ]
            if not rendered_children:
                return False
            for child in rendered_children:
                root.remove(child)
                wrapper.append(child)
            root.insert(0, wrapper)
        x, y, width, height = viewbox
        root.set("viewBox", f"{x:.8g} {y:.8g} {width:.8g} {height:.8g}")
        root.set("width", f"{width * intrinsic_scale:.8g}px")
        root.set("height", f"{height * intrinsic_scale:.8g}px")
        tree.write(output, encoding="utf-8", xml_declaration=True)
        return True
    except (ET.ParseError, OSError, ValueError):
        return False


def render_static_svg(
    sprite_dir: Path,
    rsvg_convert: str,
    max_size: int,
    zoom: float,
    scale: float,
    frame_number: int | None = None,
    slot: str | None = None,
    saved_svg_path: Path | None = None,
    preserve_game_scale: bool = False,
    png_canvas: str = "tight",
    ffdec_zoom: float = 1.0,
) -> Image.Image | None:
    """Transform one FFDec SVG frame in vector space and rasterize it once.

    Saved SVGs always retain tight visible bounds. ``png_canvas='square'``
    affects only the final raster page and does not create a second SVG.
    """
    if png_canvas not in {"tight", "square"}:
        raise ValueError(f"Unsupported PNG canvas mode: {png_canvas}")
    if ffdec_zoom <= 0:
        raise ValueError("FFDec zoom must be positive")
    frames = sorted(sprite_dir.glob("*.svg"), key=lambda path: int(path.stem))
    if frame_number is not None:
        preferred = [path for path in frames if int(path.stem) == frame_number]
        frames = preferred or frames
    for frame in frames:
        probe = probe_svg_alpha(frame, rsvg_convert)
        if probe is None:
            continue
        viewbox = visible_viewbox_from_probe(probe)
        if viewbox is None:
            continue
        transform = None
        output_viewbox = viewbox
        if preserve_game_scale:
            transform = character_item_transform(slot)
            if transform is not None and ffdec_zoom != 1:
                inverse_zoom = (
                    1 / ffdec_zoom,
                    0.0,
                    0.0,
                    1 / ffdec_zoom,
                    0.0,
                    0.0,
                )
                transform = compose_transforms(transform, inverse_zoom)
        elif is_weapon_slot(slot):
            transform, _ = weapon_svg_transform(viewbox)
        if transform is not None:
            transformed_viewbox = transformed_viewbox_from_probe(
                probe,
                transform,
            )
            if transformed_viewbox is None:
                continue
            output_viewbox = transformed_viewbox
        cropped_svg = frame.with_name(f".{frame.stem}.transformed.svg")
        rendered_png = frame.with_name(f".{frame.stem}.fitted.png")
        native_maximum = max(output_viewbox[2], output_viewbox[3])
        intrinsic_scale = 1.0
        if preserve_game_scale:
            intrinsic_scale = CHARACTER_DETAIL_DISPLAY_SCALE * scale
            if max_size > 0 and native_maximum * intrinsic_scale > max_size:
                intrinsic_scale = max_size / native_maximum
            final_maximum = max(1, round(native_maximum * intrinsic_scale))
        else:
            final_maximum = (
                max_size
                if max_size > 0
                else max(
                    1,
                    round(native_maximum * max(zoom, 1) * scale),
                )
            )
        if not write_svg_viewbox(
            frame,
            cropped_svg,
            output_viewbox,
            transform,
            intrinsic_scale,
        ):
            continue
        try:
            square_canvas_size = None
            if png_canvas == "square":
                square_canvas_size = max_size if max_size > 0 else final_maximum
            if (
                render_svg_to_maximum(
                    cropped_svg,
                    rendered_png,
                    final_maximum,
                    rsvg_convert,
                    square_canvas_size,
                )
                is None
            ):
                continue
            with Image.open(rendered_png) as rendered:
                img = rendered.convert("RGBA")
                img.load()
            bbox = alpha_bbox(img)
            # The SVG viewBox is already the tight transformed artwork bound.
            # Preserve rsvg's exact requested canvas so no second raster resize
            # is introduced merely because antialiasing leaves one clear edge.
            if bbox is None:
                continue
            if saved_svg_path is not None:
                saved_svg_path.parent.mkdir(parents=True, exist_ok=True)
                temporary_svg = saved_svg_path.with_name(
                    f".{saved_svg_path.name}.part"
                )
                try:
                    shutil.copy2(cropped_svg, temporary_svg)
                    temporary_svg.replace(saved_svg_path)
                finally:
                    temporary_svg.unlink(missing_ok=True)
            return img
        except (OSError, ValueError):
            continue
        finally:
            cropped_svg.unlink(missing_ok=True)
            rendered_png.unlink(missing_ok=True)
    return None


def render_static_png_fallback(
    swf: Path,
    metadata: dict,
    ffdec: str,
    zoom: float,
    scale: float,
    work_dir: Path,
) -> Image.Image | None:
    """Use the legacy FFDec PNG path when an SVG cannot be rasterized."""
    input_dir = work_dir / "input"
    output_dir = work_dir / "output"
    input_dir.mkdir(parents=True, exist_ok=True)
    staged_name = swf.name
    shutil.copy2(swf, input_dir / staged_name)
    selected_root = metadata.get("root")
    selected_frame = select_static_root_frame(metadata)
    selected_frames = (
        {selected_root: selected_frame} if selected_root else None
    )
    try:
        export_sprites(
            ffdec,
            zoom,
            input_dir,
            output_dir,
            (selected_root,) if selected_root else (),
            selected_frames,
            "png",
        )
    except (OSError, subprocess.SubprocessError):
        return None
    root = find_exported_root(
        output_dir / staged_name,
        swf.stem,
        selected_root,
    )
    if root is None:
        return None
    return render_static_frame(root, scale, selected_frame)


def orient_item(img: Image.Image, slot: str) -> Image.Image:
    """Apply the on-character idle orientation for a slot.

    Only weapons angle when idle (held diagonally forward/up). All other slots
    (cape, helm, pet, armor, ground, ...) render upright with no rotation.
    """
    if is_weapon_slot(slot):
        bb = img.getbbox()
        img = img if bb is None else img.crop(bb)
        # AQW's on-character weapon container mirrors the authored SWF across
        # its vertical/long axis before angling it into the held pose.
        img = ImageOps.mirror(img)
        img = img.rotate(WEAPON_ROTATION_DEG, expand=True, resample=Image.BICUBIC)
        bb = img.getbbox()
        img = img if bb is None else img.crop(bb)
    return img


def transform_raster_to_character_space(
    img: Image.Image,
    slot: str | None,
) -> Image.Image:
    """Apply a characterB slot matrix to an already-rasterized fallback."""
    matrix = character_item_transform(slot)
    if matrix is None:
        return orient_item(img, str(slot or ""))
    bbox = alpha_bbox(img)
    if bbox is not None:
        img = img.crop(bbox)
    a, b, c, d, _, _ = matrix
    determinant = a * d - b * c
    if abs(determinant) < 1e-12:
        return img

    corners = [
        transform_point((a, b, c, d, 0.0, 0.0), x, y)
        for x, y in (
            (0.0, 0.0),
            (float(img.width), 0.0),
            (0.0, float(img.height)),
            (float(img.width), float(img.height)),
        )
    ]
    min_x = math.floor(min(x for x, _ in corners))
    min_y = math.floor(min(y for _, y in corners))
    max_x = math.ceil(max(x for x, _ in corners))
    max_y = math.ceil(max(y for _, y in corners))
    output_size = (max(1, max_x - min_x), max(1, max_y - min_y))
    translate_x = -min_x
    translate_y = -min_y

    inverse_a = d / determinant
    inverse_b = -b / determinant
    inverse_c = -c / determinant
    inverse_d = a / determinant
    inverse_e = -(
        inverse_a * translate_x + inverse_c * translate_y
    )
    inverse_f = -(
        inverse_b * translate_x + inverse_d * translate_y
    )
    transformed = img.transform(
        output_size,
        Image.Transform.AFFINE,
        (
            inverse_a,
            inverse_c,
            inverse_e,
            inverse_b,
            inverse_d,
            inverse_f,
        ),
        resample=Image.Resampling.BICUBIC,
        fillcolor=(0, 0, 0, 0),
    )
    transformed_bbox = alpha_bbox(transformed)
    return (
        transformed
        if transformed_bbox is None
        else transformed.crop(transformed_bbox)
    )


def orient_frames(frames: list[Image.Image], slot: str) -> list[Image.Image]:
    """Orient a whole animation, keeping all frames on a shared canvas."""
    if not is_weapon_slot(slot):
        return frames
    rotated = []
    for f in frames:
        img = ImageOps.mirror(f).rotate(
            WEAPON_ROTATION_DEG, expand=True, resample=Image.BICUBIC
        )
        rotated.append(img)
    # Align all rotated frames to their union bbox so the animation is stable.
    x0, y0, x1, y1 = union_bbox(rotated)
    return [i.crop((x0, y0, x1, y1)) for i in rotated]


def resize_within_maximum(img: Image.Image, max_size: int) -> Image.Image:
    """Downscale an image to fit within a square maximum, preserving aspect.

    Images already within the limit are returned unchanged. The result is not
    padded to a square, so transparent PNGs retain their tightly cropped shape.
    """
    if max_size <= 0 or max(img.size) <= max_size:
        return img
    scale = max_size / max(img.size)
    target = tuple(max(1, round(dimension * scale)) for dimension in img.size)
    return img.resize(target, Image.Resampling.LANCZOS)


def resize_to_maximum(img: Image.Image, max_size: int) -> Image.Image:
    """Resize up or down so the longest side equals max_size, without padding."""
    if max_size <= 0 or max(img.size) == max_size:
        return img
    scale = max_size / max(img.size)
    target = tuple(max(1, round(dimension * scale)) for dimension in img.size)
    return img.resize(target, Image.Resampling.LANCZOS)


def center_on_square_canvas(
    img: Image.Image,
    canvas_size: int = 0,
) -> Image.Image:
    """Center an image on a transparent square without resizing its artwork."""
    side = max(canvas_size, *img.size)
    if img.size == (side, side):
        return img
    canvas = Image.new("RGBA", (side, side), (0, 0, 0, 0))
    offset = ((side - img.width) // 2, (side - img.height) // 2)
    canvas.alpha_composite(img.convert("RGBA"), offset)
    return canvas


def resize_frames_within_maximum(
    frames: Sequence[Image.Image], max_size: int
) -> list[Image.Image]:
    """Downscale animation frames by one ratio so their canvases stay aligned."""
    if not frames or max_size <= 0:
        return list(frames)
    largest_dimension = max(max(frame.size) for frame in frames)
    if largest_dimension <= max_size:
        return list(frames)
    scale = max_size / largest_dimension
    return [
        frame.resize(
            tuple(max(1, round(dimension * scale)) for dimension in frame.size),
            Image.Resampling.LANCZOS,
        )
        for frame in frames
    ]


def write_gallery(
    output_dir: Path,
    rendered: Sequence[tuple[dict, tuple[int, int], Path]],
    *,
    png_only: bool,
) -> Path:
    """Write a bounded thumbnail gallery without stretching narrow artwork."""
    gallery = output_dir / "gallery.html"
    webp_dir = output_dir / "webp"
    output_kind = "static PNG" if png_only else "animated WebP"
    with gallery.open("w", encoding="utf-8") as handle:
        handle.write(
            "<!doctype html><html><head><meta charset='utf-8'>"
            "<meta name='viewport' content='width=device-width,initial-scale=1'>"
            "<title>Rendered AQW items</title><style>"
            "body{background:#333;color:#ccc;font-family:sans-serif}"
            ".gallery{display:grid;grid-template-columns:repeat(auto-fill,minmax(170px,1fr));gap:4px}"
            "figure{margin:0;padding:6px;background:#2b2b2b;min-width:0}"
            ".preview{height:150px;display:flex;align-items:center;justify-content:center}"
            ".preview img{display:block;width:auto;height:auto;max-width:140px;max-height:140px;object-fit:contain}"
            "figcaption{color:#aaa;font-size:12px;overflow-wrap:anywhere}"
            "</style></head><body>"
            f"<h1>Rendered items ({len(rendered)}) - {output_kind}</h1>"
            "<main class='gallery'>"
        )
        for item, size, png_path in sorted(rendered, key=lambda entry: str(entry[2])):
            source_path = (
                png_path
                if png_only
                else webp_dir / f"{png_path.stem}.webp"
            )
            image_source = html.escape(
                source_path.relative_to(output_dir).as_posix(), quote=True
            )
            item_id = html.escape(str(item.get("id", "")))
            item_name = html.escape(str(item.get("name", "")))
            handle.write(
                "<figure><div class='preview'>"
                f"<img loading='lazy' src='{image_source}' alt=''>"
                "</div>"
                f"<figcaption>{item_id} {item_name} ({size[0]}x{size[1]})</figcaption>"
                "</figure>"
            )
        handle.write("</main></body></html>")
    return gallery


def is_slow_batch(
    total_seconds: float,
    seconds_per_swf: float,
    swf_count: int,
) -> bool:
    """Whether a batch exceeded its per-SWF elapsed-time allowance."""
    return (
        seconds_per_swf > 0
        and swf_count > 0
        and total_seconds > seconds_per_swf * swf_count
    )


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--database", type=Path, default=DEFAULT_DATABASE)
    parser.add_argument("--swf-dir", type=Path, default=DEFAULT_SWF_DIR)
    parser.add_argument("--output-dir", type=Path, default=DEFAULT_OUTPUT_DIR)
    parser.add_argument("--ffdec", default=None, help="Path to ffdec-cli.jar.")
    parser.add_argument("--ids", default=None, help="Comma-separated item ids.")
    parser.add_argument("--slots", nargs="+", help="Only these slots (weapon helm cape ...).")
    parser.add_argument("--limit", type=int, default=0)
    parser.add_argument(
        "--zoom",
        type=float,
        default=3,
        help=(
            "FFDec zoom (default 3). With SVG --game-scale, this is the weapon "
            "line-weight reference and other slot zooms are derived from it."
        ),
    )
    parser.add_argument(
        "--scale",
        type=float,
        default=1.0,
        metavar="MULTIPLIER",
        help=(
            "Additional positive output-size multiplier, including decimals "
            "such as 1.5 (default 1)."
        ),
    )
    parser.add_argument(
        "--max-size",
        type=int,
        default=0,
        metavar="PIXELS",
        help=(
            "Fit static output's longest side exactly to this value; animation "
            "output is only downscaled. With --game-scale this is a cap, not "
            "a forced artwork size. With --png-canvas square it is also the "
            "square page size. Zero disables the limit."
        ),
    )
    parser.add_argument(
        "--game-scale",
        action="store_true",
        help=(
            "Preserve exact per-slot transforms decompiled from characterB.swf "
            "and the official character-detail display scale. Small artwork is "
            "not enlarged to --max-size. Requires --png-only."
        ),
    )
    parser.add_argument(
        "--png-canvas",
        choices=("tight", "square"),
        default="tight",
        help=(
            "Static PNG framing: tight visible bounds (default), or centered "
            "on a transparent square. With --max-size, square output is exactly "
            "PIXELS x PIXELS. Saved SVGs always remain tight."
        ),
    )
    parser.add_argument("--no-skip-armor", action="store_true",
                        help="Attempt armor (may yield partial body parts).")
    parser.add_argument(
        "--batch-size",
        type=int,
        default=None,
        help="SWFs per FFDec process (default: 1 for --png-only, otherwise 40).",
    )
    parser.add_argument(
        "--workers",
        type=int,
        default=1,
        help="Number of FFDec batches/JVMs to render concurrently (default 1).",
    )
    parser.add_argument(
        "--skip-existing",
        action="store_true",
        help="Resume a run by keeping valid existing outputs and rendering only missing files.",
    )
    parser.add_argument(
        "--slow-log-seconds",
        type=float,
        default=1.0,
        metavar="SECONDS_PER_SWF",
        help=(
            "Log filename and FFDec/Python timing when a batch exceeds this "
            "many seconds per SWF (default 1; 0 disables)."
        ),
    )
    parser.add_argument(
        "--png-only",
        action="store_true",
        help="Render one static PNG per item without inspecting or exporting animation.",
    )
    parser.add_argument(
        "--static-renderer",
        choices=("auto", "svg", "png"),
        default="auto",
        help=(
            "Static rendering backend: SVG fits vector artwork directly to the "
            "target size, PNG uses legacy FFDec raster export, and auto prefers SVG."
        ),
    )
    parser.add_argument(
        "--save-svg",
        action="store_true",
        help=(
            "Keep each final cropped/transformed SVG in a separate sibling "
            "asset tree. Requires the static SVG renderer."
        ),
    )
    parser.add_argument(
        "--svg-output-dir",
        type=Path,
        default=None,
        metavar="PATH",
        help=(
            "Saved-SVG root (implies --save-svg; default: <output-dir>_svg)."
        ),
    )
    parser.add_argument(
        "--rsvg-convert",
        default=None,
        metavar="PATH",
        help="Path to rsvg-convert for SVG static rendering (default: find it on PATH).",
    )
    parser.add_argument(
        "--unique-swfs",
        action="store_true",
        help="Render only the first database item associated with each unique SWF file.",
    )
    parser.add_argument(
        "--include-unmapped-assets",
        action="store_true",
        help="Also render SWFs under items/ that have no item-database record.",
    )
    parser.add_argument(
        "--mirror-asset-tree",
        action="store_true",
        help=(
            "Write paths such as items/staves/example.png, mirroring the SWF "
            "tree and basename. Requires --png-only and --unique-swfs."
        ),
    )
    parser.add_argument("--idle-duration", type=int, default=40,
                        help="ms per frame for animated idle webp (default 40, matching AQW).")
    parser.add_argument(
        "--animation-start",
        type=int,
        default=1,
        help="First FFDec nested subframe to keep (1-indexed; default 1).",
    )
    parser.add_argument(
        "--animation-frames",
        type=int,
        default=100,
        help="Number of nested idle frames to keep (default 100 / 4 seconds).",
    )
    parser.add_argument(
        "--leading-frame-durations",
        type=parse_positive_int_csv,
        default=[],
        metavar="MS,MS,...",
        help="Override the first WebP frame durations; remaining frames use --idle-duration.",
    )
    parser.add_argument(
        "--sublength",
        type=int,
        default=0,
        help="Total subframes to export; defaults to animation-start + animation-frames - 1.",
    )
    args = parser.parse_args()
    save_svg = bool(args.save_svg or args.svg_output_dir is not None)

    if args.batch_size is None:
        args.batch_size = 1 if args.png_only else 40

    if args.max_size < 0:
        parser.error("--max-size cannot be negative")
    if args.zoom <= 0:
        parser.error("--zoom must be positive")
    if args.scale <= 0:
        parser.error("--scale must be positive")
    if args.game_scale and not args.png_only:
        parser.error("--game-scale requires --png-only")
    if args.png_canvas != "tight" and not args.png_only:
        parser.error("--png-canvas square requires --png-only")
    if args.batch_size < 1 or args.workers < 1:
        parser.error("--batch-size and --workers must be positive")
    if args.slow_log_seconds < 0:
        parser.error("--slow-log-seconds cannot be negative")
    if args.include_unmapped_assets and (args.ids or args.slots or args.limit):
        parser.error(
            "--include-unmapped-assets is for a full run and cannot be combined "
            "with --ids, --slots, or --limit"
        )
    if args.mirror_asset_tree and not (args.png_only and args.unique_swfs):
        parser.error(
            "--mirror-asset-tree requires --png-only and --unique-swfs"
        )
    if save_svg and not args.png_only:
        parser.error("--save-svg requires --png-only")
    if args.animation_start < 1 or args.animation_frames < 1:
        parser.error("--animation-start and --animation-frames must be positive")
    required_sublength = args.animation_start + args.animation_frames - 1
    export_sublength = args.sublength or required_sublength
    if export_sublength < required_sublength:
        parser.error(
            "--sublength is too short for the requested animation start/frame count"
        )
    try:
        build_frame_durations(
            args.animation_frames,
            args.idle_duration,
            args.leading_frame_durations,
        )
    except ValueError as exc:
        parser.error(str(exc))

    ffdec = args.ffdec or DEFAULT_FFDEC
    if not Path(ffdec).exists():
        print(f"FFDec jar not found: {ffdec}", file=__import__("sys").stderr)
        return 2

    rsvg_convert = args.rsvg_convert or shutil.which("rsvg-convert")
    if rsvg_convert and not (Path(rsvg_convert).is_file() or shutil.which(rsvg_convert)):
        rsvg_convert = None
    use_svg_static = bool(
        args.png_only
        and args.static_renderer != "png"
        and rsvg_convert
    )
    ffdec_version = detect_ffdec_version(ffdec) if use_svg_static else None
    if args.png_only and args.static_renderer == "svg" and not rsvg_convert:
        print(
            "rsvg-convert is required by --static-renderer svg but was not found.",
            file=__import__("sys").stderr,
        )
        return 2
    if save_svg and not use_svg_static:
        parser.error(
            "--save-svg requires the SVG static renderer and rsvg-convert"
        )
    if args.png_only:
        if use_svg_static:
            version_text = (
                ".".join(str(part) for part in ffdec_version)
                if ffdec_version is not None
                else "unknown"
            )
            print(
                "Static renderer: FFDec SVG with tight vector bounds and bounded "
                f"rasterization (FFDec {version_text}).",
                flush=True,
            )
        elif args.static_renderer == "auto":
            print(
                "NOTE: rsvg-convert was not found; using legacy FFDec PNG rendering.",
                flush=True,
            )
        if args.game_scale:
            print(
                "Sizing: exact characterB slot transforms at character-detail "
                f"scale {CHARACTER_DETAIL_DISPLAY_SCALE:g}; --max-size caps "
                "the artwork.",
                flush=True,
            )
            if use_svg_static:
                zoom_summary = " ".join(
                    f"{slot}={game_scale_ffdec_zoom(slot, args.zoom):.4g}"
                    for slot in (
                        "weapon",
                        "helm",
                        "cape",
                        "gauntlet",
                        "pet",
                        "ground",
                    )
                )
                print(
                    f"SVG line-weight zooms: {zoom_summary}",
                    flush=True,
                )
        if args.png_canvas == "square":
            page_size = f"{args.max_size}x{args.max_size}" if args.max_size else "natural"
            print(
                f"PNG canvas: centered transparent square ({page_size}); "
                "saved SVGs remain tight.",
                flush=True,
            )

    database = load_database(args.database)
    items = select_items(database, args)
    swf_dir = args.swf_dir
    out_dir = args.output_dir
    svg_output_dir = (
        args.svg_output_dir
        if args.svg_output_dir is not None
        else default_svg_output_dir(out_dir)
    )
    png_dir = out_dir if args.mirror_asset_tree else out_dir / "png"
    webp_dir = out_dir / "webp"
    png_dir.mkdir(parents=True, exist_ok=True)
    if save_svg:
        svg_output_dir.mkdir(parents=True, exist_ok=True)
    if not args.png_only:
        webp_dir.mkdir(parents=True, exist_ok=True)

    actual_item_assets: dict[str, Path] = {}
    if args.mirror_asset_tree:
        actual_item_assets = {
            str(asset.relative_to(swf_dir)).casefold(): asset
            for asset in (swf_dir / "items").rglob("*")
            if asset.is_file() and asset.suffix.casefold() == ".swf"
        }

    file_groups: dict[Path, list[dict]] = {}
    missing = 0
    for item in items:
        paths = resolve_local_paths(item, swf_dir)
        if not paths:
            missing += 1
            continue
        for p in paths:
            if actual_item_assets:
                try:
                    key = str(p.relative_to(swf_dir)).casefold()
                except ValueError:
                    key = ""
                p = actual_item_assets.get(key, p)
            file_groups.setdefault(p, []).append(item)
    if args.unique_swfs:
        file_groups = {path: [records[0]] for path, records in file_groups.items()}
    if args.include_unmapped_assets:
        added = add_unmapped_item_assets(file_groups, swf_dir)
        if added:
            print(f"NOTE: added {added} local SWF asset(s) with no database record.")
    if missing:
        print(f"NOTE: {missing} selected item(s) have no local SWF (skipped).")

    rendered: list[tuple[dict, tuple[int, int], Path]] = []
    unrenderable: list[dict] = []
    if args.skip_existing:
        pending_groups: dict[Path, list[dict]] = {}
        skipped_existing = 0
        for source, records in file_groups.items():
            pending_records = []
            for item in records:
                png_path = png_output_path(
                    source,
                    item,
                    swf_dir,
                    out_dir,
                    png_dir,
                    args.mirror_asset_tree,
                )
                webp_path = webp_dir / f"{item_output_stem(item)}.webp"
                outputs_exist = png_path.is_file() and png_path.stat().st_size > 0
                if save_svg:
                    svg_path = mirrored_asset_output_path(
                        source,
                        swf_dir,
                        svg_output_dir,
                        ".svg",
                    )
                    outputs_exist = (
                        outputs_exist
                        and svg_path.is_file()
                        and svg_path.stat().st_size > 0
                    )
                if not args.png_only:
                    outputs_exist = (
                        outputs_exist
                        and webp_path.is_file()
                        and webp_path.stat().st_size > 0
                    )
                if outputs_exist:
                    try:
                        with Image.open(png_path) as existing:
                            existing.load()
                            existing_size = existing.size
                    except (OSError, ValueError):
                        outputs_exist = False
                    else:
                        if args.max_size and max(existing_size) > args.max_size:
                            outputs_exist = False
                        elif (
                            args.png_canvas == "square"
                            and (
                                existing_size[0] != existing_size[1]
                                or (
                                    args.max_size > 0
                                    and existing_size
                                    != (args.max_size, args.max_size)
                                )
                            )
                        ):
                            outputs_exist = False
                        elif (
                            use_svg_static
                            and not args.game_scale
                            and args.max_size
                            and max(existing_size) != args.max_size
                        ):
                            # SVG static rendering intentionally enlarges small
                            # artwork so every result uses the requested maximum.
                            outputs_exist = False
                if outputs_exist:
                    rendered.append((item, existing_size, png_path))
                    skipped_existing += 1
                else:
                    pending_records.append(item)
            if pending_records:
                pending_groups[source] = pending_records
        file_groups = pending_groups
        print(
            f"Resume: keeping {skipped_existing} valid existing image(s); "
            f"{len(file_groups)} SWF(s) remain.",
            flush=True,
        )

    with tempfile.TemporaryDirectory(prefix="render_swf_") as tmp:
        tmp_path = Path(tmp)
        files = list(file_groups)

        # One FFDec invocation has one export zoom. Group game-scaled SVG work
        # by derived slot zoom before applying the requested batch size, so a
        # mixed weapon/pet batch cannot accidentally give both the same line
        # weight. The usual --batch-size 1 path is unchanged.
        batch_specs: list[tuple[float, list[Path]]] = []
        if use_svg_static and args.game_scale:
            zoom_groups: dict[float, list[Path]] = {}
            for asset in files:
                slot = single_record_slot(file_groups[asset])
                export_zoom = game_scale_ffdec_zoom(slot, args.zoom)
                zoom_groups.setdefault(export_zoom, []).append(asset)
            for export_zoom, grouped_files in zoom_groups.items():
                batch_specs.extend(
                    (export_zoom, grouped_files[start : start + args.batch_size])
                    for start in range(0, len(grouped_files), args.batch_size)
                )
        else:
            batch_specs.extend(
                (1.0, files[start : start + args.batch_size])
                for start in range(0, len(files), args.batch_size)
            )

        def render_batch(
            batch_number: int,
        ) -> tuple[
            int,
            list[tuple[dict, tuple[int, int], Path]],
            list[dict],
            dict,
        ]:
            batch_started = time.perf_counter()
            svg_export_zoom, chunk = batch_specs[batch_number]
            batch_rendered: list[tuple[dict, tuple[int, int], Path]] = []
            batch_unrenderable: list[dict] = []
            svg_fallback_paths: list[str] = []
            batch_dir = tmp_path / f"batch_{batch_number}"
            batch_dir.mkdir()
            staged_names: dict[Path, str] = {}
            for index, f in enumerate(chunk):
                # Different AQW asset categories occasionally contain SWFs
                # with the same basename. Give each temporary copy a unique
                # name so one cannot overwrite another inside a flat batch.
                staged_name = f"{index:04d}_{f.name}"
                staged_names[f] = staged_name
                shutil.copy2(f, batch_dir / staged_name)
            out_root = batch_dir / "out"
            selected_roots: dict[Path, int] = {}
            selected_frames: dict[int, int] = {}
            static_metadata: dict[Path, dict] = {}
            if args.png_only:
                static_metadata = {
                    asset: read_swf_sprite_metadata(asset) for asset in chunk
                }
                if len(chunk) == 1:
                    metadata = static_metadata[chunk[0]]
                    if metadata.get("root"):
                        selected_roots[chunk[0]] = metadata["root"]
                        selected_frames[metadata["root"]] = (
                            select_static_root_frame(metadata)
                        )
            svg_state_incompatible = [
                asset
                for asset, metadata in static_metadata.items()
                if not svg_static_state_is_compatible(metadata, ffdec_version)
            ]
            ffdec_started = time.perf_counter()
            batch_uses_svg = use_svg_static and not svg_state_incompatible
            if use_svg_static and svg_state_incompatible:
                svg_fallback_paths.extend(
                    str(path.relative_to(swf_dir))
                    for path in svg_state_incompatible
                )
            raster_zoom = (
                CHARACTER_DETAIL_DISPLAY_SCALE
                if args.game_scale
                else args.zoom
            )
            try:
                export_sprites(
                    ffdec,
                    svg_export_zoom if batch_uses_svg else raster_zoom,
                    batch_dir,
                    out_root,
                    tuple(selected_roots.values()),
                    selected_frames,
                    "svg" if batch_uses_svg else "png",
                )
            except (OSError, subprocess.SubprocessError):
                if not batch_uses_svg:
                    raise
                batch_uses_svg = False
                out_root = batch_dir / "out_png"
                export_sprites(
                    ffdec,
                    raster_zoom,
                    batch_dir,
                    out_root,
                    tuple(selected_roots.values()),
                    selected_frames,
                    "png",
                )
                svg_fallback_paths.extend(
                    str(path.relative_to(swf_dir)) for path in chunk
                )
            ffdec_seconds = time.perf_counter() - ffdec_started

            for file_index, f in enumerate(chunk):
                file_out = out_root / staged_names[f]
                root = find_exported_root(
                    file_out,
                    f.stem,
                    selected_roots.get(f),
                )
                info = {"animated": False}
                static_svg_transformed_slot: str | None = None
                if args.png_only:
                    metadata = static_metadata.get(f, {})
                    preferred_frame = select_static_root_frame(metadata)
                    if batch_uses_svg and root is not None:
                        svg_slot = single_record_slot(file_groups[f])
                        saved_svg_path = (
                            mirrored_asset_output_path(
                                f,
                                swf_dir,
                                svg_output_dir,
                                ".svg",
                            )
                            if save_svg
                            else None
                        )
                        if saved_svg_path is not None:
                            saved_svg_path.unlink(missing_ok=True)
                        static_frame = render_static_svg(
                            root,
                            str(rsvg_convert),
                            args.max_size,
                            args.zoom,
                            args.scale,
                            preferred_frame,
                            svg_slot,
                            saved_svg_path,
                            args.game_scale,
                            args.png_canvas,
                            svg_export_zoom,
                        )
                        if static_frame is not None and (
                            (
                                args.game_scale
                                and character_item_transform(svg_slot) is not None
                            )
                            or (not args.game_scale and is_weapon_slot(svg_slot))
                        ):
                            static_svg_transformed_slot = svg_slot
                    elif root is not None:
                        static_frame = render_static_frame(
                            root, args.scale, preferred_frame
                        )
                    else:
                        static_frame = None
                    if static_frame is None and batch_uses_svg:
                        static_svg_transformed_slot = None
                        fallback_started = time.perf_counter()
                        static_frame = render_static_png_fallback(
                            f,
                            metadata,
                            ffdec,
                            raster_zoom,
                            args.scale,
                            batch_dir / f"fallback_{file_index}",
                        )
                        ffdec_seconds += time.perf_counter() - fallback_started
                        if static_frame is not None:
                            svg_fallback_paths.append(
                                str(f.relative_to(swf_dir))
                            )
                    frames = [static_frame] if static_frame is not None else None
                else:
                    if root is None:
                        batch_unrenderable.extend(file_groups[f])
                        continue
                    info = parse_idle_info(f, ffdec)
                if not args.png_only and info.get("animated"):
                    # AS-driven pet/cape: export the Idle root frame with sublength
                    sub_path = (
                        tmp_path
                        / f"anim_{batch_number}_{file_index}_{f.stem}"
                    )
                    subs = export_idle_animated(
                        ffdec, args.zoom, f, info["root"], info["first"],
                        sub_path, export_sublength,
                    )
                    if subs:
                        try:
                            subs = select_subframes(
                                subs, args.animation_start, args.animation_frames
                            )
                        except ValueError:
                            subs = None
                    if subs:
                        anim_frames = []
                        for pf in subs:
                            try:
                                im = Image.open(pf).convert("RGBA")
                            except Exception:
                                continue
                            if im.getbbox():
                                anim_frames.append(im)
                        frames = align_images(anim_frames, args.scale)
                    else:
                        frames = render_frames(root, args.scale, (info["first"], info["last"]))
                elif not args.png_only:
                    # Static / pure-timeline item using root frames
                    frames = render_frames(root, args.scale, None)
                if not frames:
                    batch_unrenderable.extend(file_groups[f])
                    continue
                for item in file_groups[f]:
                    slot = item.get("slot")
                    normalized_slot = str(slot or "").strip().casefold()
                    if static_svg_transformed_slot == normalized_slot:
                        img_item = frames[0]
                    elif args.game_scale:
                        img_item = transform_raster_to_character_space(
                            frames[0],
                            slot,
                        )
                    else:
                        img_item = orient_item(frames[0], slot)
                    if args.png_only:
                        resize_static = (
                            resize_within_maximum
                            if args.game_scale
                            else resize_to_maximum
                        )
                        img_item = resize_static(img_item, args.max_size)
                        if args.png_canvas == "square":
                            img_item = center_on_square_canvas(
                                img_item,
                                args.max_size,
                            )
                        anim = []
                    else:
                        anim = orient_frames(frames, slot)
                        img_item = resize_within_maximum(img_item, args.max_size)
                        anim = resize_frames_within_maximum(anim, args.max_size)
                    stem = item_output_stem(item)
                    png_path = png_output_path(
                        f,
                        item,
                        swf_dir,
                        out_dir,
                        png_dir,
                        args.mirror_asset_tree,
                    )
                    webp_path = webp_dir / f"{stem}.webp"
                    png_path.parent.mkdir(parents=True, exist_ok=True)
                    img_item.save(png_path)
                    if not args.png_only:
                        leading = args.leading_frame_durations if info.get("animated") else []
                        durations = build_frame_durations(
                            len(anim), args.idle_duration, leading
                        )
                        save_lossless_webp(anim, webp_path, durations)
                    batch_rendered.append((item, img_item.size, png_path))
            total_seconds = time.perf_counter() - batch_started
            timing = {
                "paths": [str(path.relative_to(swf_dir)) for path in chunk],
                "total": total_seconds,
                "ffdec": ffdec_seconds,
                "python": total_seconds - ffdec_seconds,
                "svg_fallbacks": svg_fallback_paths,
            }
            return len(chunk), batch_rendered, batch_unrenderable, timing

        starts = list(range(len(batch_specs)))
        if args.png_only and args.batch_size == 1:
            print(
                "Root-targeted mode: FFDec will export only each SWF's selected root frame.",
                flush=True,
            )
        print(
            f"Rendering {len(files)} SWFs in {len(starts)} batch(es) "
            f"with {args.workers} worker(s).",
            flush=True,
        )
        completed_swfs = 0

        def absorb_batch(
            result: tuple[
                int,
                list[tuple[dict, tuple[int, int], Path]],
                list[dict],
                dict,
            ]
        ) -> None:
            nonlocal completed_swfs
            processed, new_rendered, new_unrenderable, timing = result
            completed_swfs += processed
            rendered.extend(new_rendered)
            unrenderable.extend(new_unrenderable)
            print(
                f"Progress: SWFs={completed_swfs}/{len(files)} "
                f"images={len(rendered)} unrenderable={len(unrenderable)}",
                flush=True,
            )
            for fallback_path in timing["svg_fallbacks"]:
                print(
                    f"SVG fallback: {fallback_path} used legacy FFDec PNG rendering.",
                    flush=True,
                )
            if is_slow_batch(
                timing["total"], args.slow_log_seconds, processed
            ):
                names = ", ".join(timing["paths"])
                threshold = args.slow_log_seconds * processed
                unit = "SWF" if processed == 1 else "SWFs"
                print(
                    f"Slow batch: {processed} {unit} threshold={threshold:.3f}s "
                    f"paths={names} total={timing['total']:.3f}s "
                    f"ffdec={timing['ffdec']:.3f}s "
                    f"python={timing['python']:.3f}s",
                    flush=True,
                )

        if args.workers == 1:
            for start in starts:
                absorb_batch(render_batch(start))
        else:
            # Keep only one batch per worker in flight. This bounds temporary
            # disk use and lets a failure cancel queued work promptly.
            starts_iter = iter(starts)
            with ThreadPoolExecutor(max_workers=args.workers) as executor:
                in_flight = {
                    executor.submit(render_batch, start): start
                    for start in [next(starts_iter, None) for _ in range(args.workers)]
                    if start is not None
                }
                while in_flight:
                    future = next(as_completed(in_flight))
                    in_flight.pop(future)
                    try:
                        absorb_batch(future.result())
                    except Exception:
                        for pending in in_flight:
                            pending.cancel()
                        raise
                    next_start = next(starts_iter, None)
                    if next_start is not None:
                        in_flight[executor.submit(render_batch, next_start)] = next_start

    gallery = write_gallery(out_dir, rendered, png_only=args.png_only)

    print(f"Rendered {len(rendered)} item image(s) to {out_dir}")
    if unrenderable:
        print(f"{len(unrenderable)} item(s) not rendered (no single root sprite, likely armor/parts):")
        for item in unrenderable[:8]:
            print(f"   id={item.get('id')} {item.get('name')}")
        if len(unrenderable) > 8:
            print(f"   ... and {len(unrenderable) - 8} more")
    print(f"Gallery: {gallery}")
    if save_svg:
        print(f"SVGs: {svg_output_dir}")
    return 0


if __name__ == "__main__":
    import sys
    sys.exit(main())
