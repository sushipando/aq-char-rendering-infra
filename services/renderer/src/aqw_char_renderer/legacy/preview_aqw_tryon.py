#!/usr/bin/env python3
"""Render an AQW character with one local SWF item override using native AIR.

This is a command-line proof of concept for a future Discord try-on feature.
It fetches the public FlashVars for a username, substitutes one armor, weapon,
helm, cape, or pet, stages original SWF assets locally, and lets AQW's official
``characterB.swf`` compose the result under Harman AIR. The output is a
transparent PNG rendered by the native Flash/AIR vector and ActionScript
runtime.

Example:

    ./venv/bin/python pipeline/preview_aqw_tryon.py Tdnq \
      --slot weapon \
      --swf bot/assets/swf_item_index/swf_assets/items/daggers/CursedClaws.swf \
      --link CursedClaws \
      --weapon-type Dagger

The SWF's exported root symbol (AQW's ``sLink``) is inferred when unambiguous.
Pass ``--link`` explicitly when inference reports multiple candidates.
"""

from __future__ import annotations

import argparse
from difflib import SequenceMatcher
import hashlib
from html.parser import HTMLParser
import json
import os
from pathlib import Path, PurePosixPath
import re
import shutil
import struct
import subprocess
import sys
import tempfile
from typing import Any, Iterable
from urllib.error import HTTPError, URLError
from urllib.parse import parse_qsl, quote, urlencode
from urllib.request import Request, urlopen
import uuid
import zlib


REPO_ROOT = Path(__file__).resolve().parents[5]
AIR_SOURCE_DIR = Path(__file__).with_name("aqw_tryon_air")
DEFAULT_ASSET_ROOT = (
    REPO_ROOT / "bot" / "assets" / "swf_item_index" / "swf_assets"
)
DEFAULT_DATABASE = (
    REPO_ROOT / "bot" / "assets" / "swf_item_index" / "item_db.json"
)
DEFAULT_BUILD_DIR = REPO_ROOT / "tmp" / "aqw_tryon_air"
DEFAULT_OUTPUT_DIR = REPO_ROOT / "render_outputs" / "tryon"
DEFAULT_FFDEC = Path(
    "/Applications/FFDec.app/Contents/Resources/ffdec-cli.jar"
)
DEFAULT_CHARACTER_RENDERER = (
    Path.home() / "Projects" / "aq" / "swf" / "characterB.swf"
)

CHARACTER_PAGE_URL = "https://account.aq.com/CharPage"
FALLBACK_FVARS_URL = "https://game.aq.com/game/api/charpage/fvars"
GAMEFILES_URL = "https://game.aq.com/game/gamefiles/"
CHARACTER_RENDERER_URL = (
    "https://game.aq.com/game/gamefiles/etc/chardetail/characterB.swf?v=2"
)
USER_AGENT = "aqw-char-renderer/0.1"
SWF_SIGNATURES = {b"FWS", b"CWS", b"ZWS"}
PATCH_VERSION = "native-air-v2"
SUPPORTED_SLOTS = {"armor", "weapon", "helm", "cape", "pet"}
WEAPON_DATABASE_SLOTS = {"weapon", "gauntlet", "handgun", "rifle", "whip"}


class TryOnError(RuntimeError):
    """Raised when a native try-on request cannot be prepared or rendered."""


class FlashVarsParser(HTMLParser):
    """Extract the first FlashVars value from an AQW character page."""

    def __init__(self) -> None:
        super().__init__(convert_charrefs=True)
        self.flashvars: str | None = None

    def handle_starttag(
        self,
        tag: str,
        attrs: list[tuple[str, str | None]],
    ) -> None:
        values = dict(attrs)
        if tag == "param" and (values.get("name") or "").casefold() == "flashvars":
            self.flashvars = values.get("value")
        elif tag == "embed" and self.flashvars is None:
            self.flashvars = values.get("flashvars")


def fetch_text(url: str, *, timeout: float) -> str:
    request = Request(url, headers={"User-Agent": USER_AGENT})
    with urlopen(request, timeout=timeout) as response:
        return response.read().decode("utf-8", errors="replace")


def parse_flashvars(encoded: str, username: str) -> dict[str, str]:
    value = encoded[1:] if encoded.startswith("&") else encoded
    fields = dict(parse_qsl(value, keep_blank_values=True))
    if "strName" not in fields:
        raise TryOnError(f"AQW returned no public character data for {username!r}")
    return fields


def parse_character_page(page: str, username: str) -> dict[str, str] | None:
    parser = FlashVarsParser()
    parser.feed(page)
    if not parser.flashvars:
        return None
    return parse_flashvars(parser.flashvars, username)


def fetch_character_flashvars(
    username: str,
    *,
    timeout: float = 15.0,
) -> dict[str, str]:
    """Fetch public character FlashVars without an AQW login."""
    query = urlencode({"id": username})
    page_url = f"{CHARACTER_PAGE_URL}?{query}"
    try:
        page = fetch_text(page_url, timeout=timeout)
    except HTTPError as error:
        # account.aq.com rejects AWS Lambda egress with 403, while AQW's
        # official game API remains the supported FlashVars fallback.
        if error.code not in {403, 404}:
            raise
        page = ""

    fields = parse_character_page(page, username) if page else None
    if fields is not None:
        return fields

    body = fetch_text(f"{FALLBACK_FVARS_URL}?{query}", timeout=timeout).strip()
    if body in {"Hidden", "Empty", ""}:
        status = body.lower() or "empty"
        raise TryOnError(f"AQW character {username!r} is {status}")
    return parse_flashvars(body, username)


def normalize_asset_path(raw_value: Any) -> str:
    """Return a safe SWF path relative to AQW's gamefiles directory."""
    value = str(raw_value or "").strip().replace("\\", "/")
    value = value.split("?", 1)[0].split("#", 1)[0].strip().lstrip("/")
    if value.casefold().startswith("gamefiles/"):
        value = value[len("gamefiles/") :]
    if not value or "://" in value or value.startswith("//"):
        raise TryOnError(f"Unsafe or empty AQW asset path: {raw_value!r}")

    parts = PurePosixPath(value).parts
    if not parts or any(part in {"", ".", ".."} for part in parts):
        raise TryOnError(f"Unsafe AQW asset path: {raw_value!r}")
    if any(":" in part or "\x00" in part for part in parts):
        raise TryOnError(f"Unsafe AQW asset path: {raw_value!r}")
    normalized = "/".join(parts)
    if not normalized.casefold().endswith(".swf"):
        raise TryOnError(f"AQW asset is not a SWF: {raw_value!r}")
    return normalized


def has_valid_swf_header(path: Path) -> bool:
    try:
        if path.stat().st_size < 8:
            return False
        with path.open("rb") as handle:
            return handle.read(3) in SWF_SIGNATURES
    except OSError:
        return False


def download_swf(url: str, destination: Path, *, timeout: float) -> None:
    """Download one validated SWF, replacing the destination atomically."""
    destination.parent.mkdir(parents=True, exist_ok=True)
    temporary = destination.with_name(f".{destination.name}.{os.getpid()}.part")
    request = Request(
        url,
        headers={
            "User-Agent": USER_AGENT,
            "Accept": (
                "application/x-shockwave-flash,"
                "application/octet-stream;q=0.9,*/*;q=0.1"
            ),
        },
    )
    try:
        with urlopen(request, timeout=timeout) as response:
            payload = response.read()
        if len(payload) < 8 or payload[:3] not in SWF_SIGNATURES:
            raise TryOnError(f"Downloaded response is not a SWF: {url}")
        temporary.write_bytes(payload)
        os.replace(temporary, destination)
    finally:
        temporary.unlink(missing_ok=True)


def decompressed_swf(path: Path) -> bytes:
    data = path.read_bytes()
    if len(data) < 12 or data[:3] not in SWF_SIGNATURES:
        raise TryOnError(f"Not a valid SWF file: {path}")
    if data[:3] == b"FWS":
        return data
    if data[:3] == b"CWS":
        try:
            body = zlib.decompress(data[8:])
        except zlib.error as error:
            raise TryOnError(f"Unable to decompress SWF {path}: {error}") from error
        return b"FWS" + data[3:8] + body
    raise TryOnError(
        f"Cannot infer symbols from LZMA-compressed SWF {path}; pass --link"
    )


def symbol_class_entries(path: Path) -> list[tuple[int, str]]:
    """Read SymbolClass tag entries from an FWS/CWS file."""
    data = decompressed_swf(path)
    nbits = data[8] >> 3
    rect_bytes = (5 + 4 * nbits + 7) // 8
    position = 8 + rect_bytes + 4  # RECT + frame rate + frame count
    entries: list[tuple[int, str]] = []

    while position + 2 <= len(data):
        tag_header = struct.unpack_from("<H", data, position)[0]
        position += 2
        tag_code = tag_header >> 6
        length = tag_header & 0x3F
        if length == 0x3F:
            if position + 4 > len(data):
                break
            length = struct.unpack_from("<I", data, position)[0]
            position += 4
        end = position + length
        if end > len(data):
            break

        if tag_code == 76 and length >= 2:  # SymbolClass
            cursor = position
            count = struct.unpack_from("<H", data, cursor)[0]
            cursor += 2
            for _ in range(count):
                if cursor + 2 > end:
                    break
                character_id = struct.unpack_from("<H", data, cursor)[0]
                cursor += 2
                terminator = data.find(b"\x00", cursor, end)
                if terminator < 0:
                    break
                name = data[cursor:terminator].decode("utf-8", errors="replace")
                entries.append((character_id, name))
                cursor = terminator + 1
        position = end
        if tag_code == 0:
            break
    return entries


def infer_export_link(path: Path, *, slot: str, gender: str) -> str:
    """Infer AQW's exported ``sLink`` when the SWF makes it unambiguous."""
    entries = symbol_class_entries(path)
    names = list(dict.fromkeys(name for _, name in entries if name))
    if not names:
        raise TryOnError(f"No exported SymbolClass names found in {path}; pass --link")

    stem = path.stem
    stem_variants = [stem]
    without_revision = re.sub(r"(?i)(?:[-_]?r\d+)$", "", stem)
    if without_revision != stem:
        stem_variants.append(without_revision)
    without_date = re.sub(r"-\d{1,2}[A-Za-z]{3}\d{2,4}$", "", stem)
    if without_date not in stem_variants:
        stem_variants.append(without_date)

    if slot == "armor":
        suffix = f"{gender.upper()}Chest"
        armor_roots = {
            name[: -len(suffix)]
            for name in names
            if name.casefold().endswith(suffix.casefold())
            and len(name) > len(suffix)
        }
        if len(armor_roots) == 1:
            return next(iter(armor_roots))
        if armor_roots:
            for candidate in stem_variants:
                matches = [
                    root
                    for root in armor_roots
                    if root.casefold() == candidate.casefold()
                ]
                if len(matches) == 1:
                    return matches[0]

            normalized_stem = re.sub(r"[^a-z0-9]", "", without_revision.casefold())
            standard_parts = (
                "Chest",
                "Hip",
                "FootIdle",
                "Foot",
                "Shoulder",
                "Hand",
                "Thigh",
                "Shin",
                "Head",
                "Robe",
                "RobeBack",
            )
            folded_names = {name.casefold() for name in names}
            ranked: list[tuple[tuple[int, int, float], str]] = []
            for root in armor_roots:
                root_token = re.sub(r"[^a-z0-9]", "", root.casefold())
                contains = int(
                    bool(root_token)
                    and (root_token in normalized_stem or normalized_stem in root_token)
                )
                part_count = sum(
                    f"{root}{gender.upper()}{part}".casefold() in folded_names
                    for part in standard_parts
                )
                similarity = SequenceMatcher(None, normalized_stem, root_token).ratio()
                ranked.append(((contains, part_count, similarity), root))
            ranked.sort(reverse=True)
            if len(ranked) == 1 or ranked[0][0] > ranked[1][0]:
                # Chest, hip, both feet, shoulder, hand, thigh, and shin are
                # characterB's eight required parts. Refuse a weaker guess.
                if ranked[0][0][1] >= 8:
                    return ranked[0][1]

    for candidate in stem_variants:
        matches = [name for name in names if name.casefold() == candidate.casefold()]
        if len(matches) == 1:
            return matches[0]

    document_classes = list(
        dict.fromkeys(name for character_id, name in entries if character_id == 0)
    )
    if len(document_classes) == 1:
        return document_classes[0]

    root_candidates = [
        name
        for name in names
        if "." not in name
        and "::" not in name
        and not name.casefold().endswith(("_backhair", "hairback"))
    ]
    if len(root_candidates) == 1:
        return root_candidates[0]

    preview = ", ".join(root_candidates[:8] or names[:8])
    if len(root_candidates or names) > 8:
        preview += ", ..."
    raise TryOnError(
        f"Could not infer one exported link from {path.name}; "
        f"pass --link explicitly. Candidates: {preview}"
    )


def effective_slot(database_slot: str) -> str:
    value = database_slot.strip().casefold()
    if value in WEAPON_DATABASE_SLOTS:
        return "weapon"
    return value


def infer_weapon_type(source: Path, database_slot: str = "") -> str:
    parts = {part.casefold() for part in source.parts}
    slot = database_slot.casefold()
    if slot == "gauntlet" or "gauntlets" in parts:
        return "Gauntlet"
    if "daggers" in parts:
        return "Dagger"
    return "Sword"


def load_item(database: Path, item_id: int) -> dict[str, Any]:
    try:
        payload = json.loads(database.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise TryOnError(f"Unable to read item database {database}: {error}") from error
    matches = [record for record in payload if record.get("id") == item_id]
    if not matches:
        raise TryOnError(f"Item ID {item_id} is not present in {database}")
    return matches[0]


class LocalAssetResolver:
    """Resolve remote AQW paths against the downloaded case-preserving tree."""

    def __init__(self, root: Path) -> None:
        self.root = root
        self._casefold_index: dict[str, Path] | None = None

    def _index(self) -> dict[str, Path]:
        if self._casefold_index is None:
            self._casefold_index = {}
            if self.root.exists():
                for path in self.root.rglob("*.swf"):
                    relative = path.relative_to(self.root).as_posix().casefold()
                    self._casefold_index.setdefault(relative, path)
        return self._casefold_index

    def resolve(self, remote_path: str) -> Path | None:
        normalized = normalize_asset_path(remote_path)
        exact = self.root.joinpath(*PurePosixPath(normalized).parts)
        if exact.is_file() and has_valid_swf_header(exact):
            return exact
        candidate = self._index().get(normalized.casefold())
        if candidate is not None and has_valid_swf_header(candidate):
            return candidate
        return None


def displayed_asset_paths(fields: dict[str, str]) -> list[str]:
    """Return only the assets characterB will load for the current appearance."""
    gender = fields.get("strGender", "M").upper()
    try:
        visibility = int(fields.get("ia1", "0") or 0)
    except ValueError:
        visibility = 0

    def chosen(slot: str, base_file: str) -> str | None:
        cosmetic_name = f"strCust{slot}Name"
        cosmetic_file = f"strCust{slot}File"
        if cosmetic_name in fields:
            return fields.get(cosmetic_file)
        return fields.get(base_file)

    paths: list[str] = []
    armor = chosen("Armor", "strClassFile")
    if armor and armor.casefold() != "none":
        paths.append(f"classes/{gender}/{armor}")

    weapon = chosen("Weapon", "strWeaponFile")
    if weapon and weapon.casefold() != "none":
        paths.append(weapon)

    helm = chosen("Helm", "strHelmFile")
    helm_visible = (visibility & (1 << 1)) == 0
    if helm_visible and helm and helm.casefold() != "none":
        paths.append(helm)
    else:
        hair = fields.get("strHairFile")
        if hair and hair.casefold() != "none":
            paths.append(hair)

    cape = chosen("Cape", "strCapeFile")
    if (visibility & 1) == 0 and cape and cape.casefold() != "none":
        paths.append(cape)

    pet = fields.get("strPetFile")
    if (visibility & (1 << 2)) == 0 and pet and pet.casefold() != "none":
        paths.append(pet)

    misc = fields.get("strMiscFile")
    if misc and misc.casefold() != "none":
        paths.append(misc)

    return list(dict.fromkeys(normalize_asset_path(path) for path in paths))


def apply_override(
    fields: dict[str, str],
    *,
    slot: str,
    swf: Path,
    link: str,
    item_name: str,
    weapon_type: str | None,
) -> str:
    """Apply one local SWF override and return its staged relative path."""
    if slot not in SUPPORTED_SLOTS:
        raise TryOnError(f"Unsupported on-character slot: {slot}")
    if not has_valid_swf_header(swf):
        raise TryOnError(f"Override is not a valid SWF: {swf}")
    if not link or any(character in link for character in "\x00\r\n"):
        raise TryOnError("The exported SWF link cannot be empty or multiline")

    filename = swf.name
    if slot == "armor":
        gender = fields.get("strGender", "M").upper()
        fields["strCustArmorFile"] = filename
        fields["strCustArmorLink"] = link
        fields["strCustArmorName"] = item_name
        return f"classes/{gender}/{filename}"

    if slot == "pet":
        staged = f"overrides/pet/{filename}"
        fields["strPetFile"] = staged
        fields["strPetLink"] = link
        fields["strPetName"] = item_name
        visibility_bit = 2
    else:
        title = slot.capitalize()
        staged = f"overrides/{slot}/{filename}"
        fields[f"strCust{title}File"] = staged
        fields[f"strCust{title}Link"] = link
        fields[f"strCust{title}Name"] = item_name
        visibility_bit = {"cape": 0, "helm": 1}.get(slot)
        if slot == "weapon":
            fields["strCustWeaponType"] = weapon_type or "Sword"

    if visibility_bit is not None:
        try:
            flags = int(fields.get("ia1", "0") or 0)
        except ValueError:
            flags = 0
        fields["ia1"] = str(flags & ~(1 << visibility_bit))
    return staged


def stage_assets(
    fields: dict[str, str],
    destination: Path,
    *,
    resolver: LocalAssetResolver,
    explicit_sources: dict[str, Path],
    timeout: float,
) -> list[str]:
    """Stage every SWF the native renderer will load for this appearance."""
    staged: list[str] = []
    explicit = {
        normalize_asset_path(path).casefold(): source
        for path, source in explicit_sources.items()
    }
    for remote_path in displayed_asset_paths(fields):
        target = destination.joinpath(*PurePosixPath(remote_path).parts)
        source = explicit.get(remote_path.casefold()) or resolver.resolve(remote_path)
        target.parent.mkdir(parents=True, exist_ok=True)
        if source is not None:
            shutil.copy2(source, target)
        else:
            url = f"{GAMEFILES_URL}{quote(remote_path, safe='/')}"
            print(f"Downloading missing character asset: {remote_path}")
            download_swf(url, target, timeout=timeout)
        if not has_valid_swf_header(target):
            raise TryOnError(f"Staged file is not a valid SWF: {target}")
        staged.append(remote_path)
    return staged


def resolve_air_sdk(value: Path | None) -> Path:
    candidates: list[Path] = []
    if value is not None:
        candidates.append(value.expanduser())
    if os.environ.get("AIR_HOME"):
        candidates.append(Path(os.environ["AIR_HOME"]).expanduser())
    candidates.append(Path.home() / "Projects" / "aq" / "airsdks" / "AIRSDK_51.3.1")
    adl = shutil.which("adl")
    if adl:
        candidates.append(Path(adl).resolve().parents[1])

    for candidate in candidates:
        if (candidate / "bin" / "adl").is_file() and (
            candidate / "bin" / "amxmlc"
        ).is_file():
            return candidate.resolve()
    raise TryOnError(
        "Harman AIR SDK not found. Pass --air-sdk or set AIR_HOME."
    )


def resolve_ffdec(value: Path | None) -> Path:
    candidates = [value.expanduser()] if value is not None else []
    if os.environ.get("FFDEC_JAR"):
        candidates.append(Path(os.environ["FFDEC_JAR"]).expanduser())
    candidates.append(DEFAULT_FFDEC)
    for candidate in candidates:
        if candidate.is_file():
            return candidate.resolve()
    raise TryOnError("FFDec CLI jar not found. Pass --ffdec with ffdec-cli.jar.")


def run_checked(command: list[str], *, cwd: Path | None = None) -> None:
    result = subprocess.run(
        command,
        cwd=cwd,
        text=True,
        capture_output=True,
        check=False,
    )
    if result.returncode != 0:
        details = "\n".join(part.strip() for part in (result.stdout, result.stderr) if part.strip())
        raise TryOnError(
            f"Command failed ({result.returncode}): {' '.join(command)}"
            + (f"\n{details}" if details else "")
        )


def replace_once(text: str, old: str, new: str, *, label: str) -> str:
    count = text.count(old)
    if count != 1:
        raise TryOnError(
            f"Expected one {label} pattern in characterB source, found {count}. "
            "AQW may have changed the renderer."
        )
    return text.replace(old, new, 1)


def patch_renderer_sources(export_dir: Path) -> Path:
    main_matches = list(export_dir.rglob("spider_characterA_fla/MainTimeline.as"))
    avatar_matches = list(export_dir.rglob("AvatarMC.as"))
    if len(main_matches) != 1 or len(avatar_matches) != 1:
        raise TryOnError("FFDec export did not contain the expected characterB scripts")
    main_path = main_matches[0]
    avatar_path = avatar_matches[0]

    main_text = main_path.read_text(encoding="utf-8")
    main_text = replace_once(
        main_text,
        '         ExternalInterface.addCallback("closeOverlay",this.closeOverlay);\n'
        '         ExternalInterface.addCallback("enableModal",this.enableModal);',
        '         if(ExternalInterface.available)\n'
        '         {\n'
        '            ExternalInterface.addCallback("closeOverlay",this.closeOverlay);\n'
        '            ExternalInterface.addCallback("enableModal",this.enableModal);\n'
        '         }',
        label="ExternalInterface callback",
    )
    hair_call = "            this.pMC.loadHair();"
    hair_call_count = main_text.count(hair_call)
    if hair_call_count != 3:
        raise TryOnError(
            "Expected three hair-loading patterns in characterB source, found "
            f"{hair_call_count}. AQW may have changed the renderer."
        )
    main_text = main_text.replace(
        hair_call,
        '            if(this.objChar.strHairFile != "none")\n'
        "            {\n"
        "               this.pMC.loadHair();\n"
        "            }",
    )
    main_path.write_text(main_text, encoding="utf-8")

    avatar_text = avatar_path.read_text(encoding="utf-8")
    avatar_text = replace_once(
        avatar_text,
        '         var reg:RegExp = /https?:\\/\\/[\\w.]+/i;\n'
        '         this.serverFilePath = MovieClip(stage.getChildAt(0)).serverFilePath.match(reg) + "/game/gamefiles/";',
        '         this.serverFilePath = MovieClip(stage.getChildAt(0)).serverFilePath;',
        label="remote asset base",
    )
    avatar_text = replace_once(
        avatar_text,
        '         AssetClass = getDefinitionByName(this.helmLink + "_backhair") as Class;\n'
        '         if(AssetClass != null)\n'
        '         {\n'
        '            this.mcChar.backhair.visible = true;\n'
        '            this.mcChar.backhair.addChild(new AssetClass());\n'
        '         }',
        '         try\n'
        '         {\n'
        '            AssetClass = getDefinitionByName(this.helmLink + "_backhair") as Class;\n'
        '            this.mcChar.backhair.visible = true;\n'
        '            this.mcChar.backhair.addChild(new AssetClass());\n'
        '         }\n'
        '         catch(err:Error)\n'
        '         {\n'
        '         }',
        label="optional helm back-hair",
    )
    avatar_text = replace_once(
        avatar_text,
        "         if(this.pAV.objData.strMiscFile == undefined)",
        '         if(this.pAV.objData.strMiscFile == undefined || this.pAV.objData.strMiscFile == "none")',
        label="optional misc asset",
    )
    avatar_path.write_text(avatar_text, encoding="utf-8")
    return avatar_path.parent


def renderer_source(
    requested: Path | None,
    *,
    build_dir: Path,
    timeout: float,
) -> Path:
    if requested is not None:
        source = requested.expanduser().resolve()
        if not has_valid_swf_header(source):
            raise TryOnError(f"Invalid character renderer SWF: {source}")
        return source
    if has_valid_swf_header(DEFAULT_CHARACTER_RENDERER):
        return DEFAULT_CHARACTER_RENDERER.resolve()

    cached = build_dir / "original-characterB.swf"
    if not has_valid_swf_header(cached):
        print("Downloading AQW's official characterB.swf...")
        download_swf(CHARACTER_RENDERER_URL, cached, timeout=timeout)
    return cached


def build_native_runtime(
    *,
    build_dir: Path,
    air_sdk: Path,
    ffdec: Path,
    original_renderer: Path,
) -> tuple[Path, Path]:
    """Build/cache the AIR host and minimally patched official renderer."""
    build_dir.mkdir(parents=True, exist_ok=True)
    host_source = AIR_SOURCE_DIR / "TryOnHost.as"
    host_output = build_dir / "TryOnHost.swf"
    renderer_output = build_dir / "characterB-air.swf"
    manifest_path = build_dir / "build.json"
    source_hash = hashlib.sha256(original_renderer.read_bytes()).hexdigest()
    host_hasher = hashlib.sha256()
    for source in sorted(AIR_SOURCE_DIR.glob("*.as")):
        host_hasher.update(source.name.encode("utf-8"))
        host_hasher.update(b"\x00")
        host_hasher.update(source.read_bytes())
        host_hasher.update(b"\x00")
    host_hash = host_hasher.hexdigest()
    desired = {
        "patch_version": PATCH_VERSION,
        "renderer_sha256": source_hash,
        "host_sha256": host_hash,
        "air_sdk": str(air_sdk),
        "ffdec": str(ffdec),
    }
    try:
        current = json.loads(manifest_path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        current = None
    if current == desired and host_output.is_file() and has_valid_swf_header(
        renderer_output
    ):
        return host_output, renderer_output

    print("Building native AIR try-on runtime...")
    with tempfile.TemporaryDirectory(prefix="aqw_tryon_build_") as temporary:
        temporary_path = Path(temporary)
        export_dir = temporary_path / "export"
        ffdec_home = temporary_path / "ffdec-home"
        ffdec_home.mkdir()
        java_prefix = [
            "java",
            "-Djava.awt.headless=true",
            f"-Duser.home={ffdec_home}",
            "-jar",
            str(ffdec),
        ]
        run_checked(
            java_prefix
            + ["-onerror", "abort", "-export", "script", str(export_dir), str(original_renderer)]
        )
        scripts_root = patch_renderer_sources(export_dir)
        temporary_renderer = temporary_path / "characterB-air.swf"
        run_checked(
            java_prefix
            + [
                "-onerror",
                "abort",
                "-importScript",
                str(original_renderer),
                str(temporary_renderer),
                str(scripts_root),
            ]
        )
        if not has_valid_swf_header(temporary_renderer):
            raise TryOnError("FFDec did not produce a valid patched character renderer")
        shutil.copy2(temporary_renderer, renderer_output)

    run_checked(
        [
            str(air_sdk / "bin" / "amxmlc"),
            "-output",
            str(host_output),
            str(host_source),
        ]
    )
    if not has_valid_swf_header(host_output):
        raise TryOnError("AIR compiler did not produce a valid TryOnHost.swf")
    manifest_path.write_text(json.dumps(desired, indent=2) + "\n", encoding="utf-8")
    return host_output, renderer_output


def adl_command(air_sdk: Path, descriptor: Path, runtime_dir: Path) -> list[str]:
    command = [
        str(air_sdk / "bin" / "adl"),
        "-nodebug",
        descriptor.name,
        ".",
    ]
    if sys.platform.startswith("linux") and not os.environ.get("DISPLAY"):
        xvfb = shutil.which("xvfb-run")
        if xvfb is None:
            raise TryOnError(
                "Headless Linux rendering requires xvfb-run (install the xvfb package)."
            )
        command = [xvfb, "-a"] + command
    return command


def render_native(
    fields: dict[str, str],
    *,
    override_path: str,
    override_swf: Path,
    resolver: LocalAssetResolver,
    output: Path,
    host_swf: Path,
    renderer_swf: Path,
    air_sdk: Path,
    capture_mode: str,
    max_size: int,
    padding: int,
    wait_seconds: float,
    timeout: float,
    verbose: bool,
    ready_mode: str | None = None,
    blank_character: bool = False,
    svg_output: Path | None = None,
    svg_intrinsic_scale: float = 1.0,
    svg_stroke_zoom: float = 1.0,
) -> dict[str, Any]:
    output = output.expanduser().resolve()
    output.parent.mkdir(parents=True, exist_ok=True)
    if svg_output is not None:
        svg_output = svg_output.expanduser().resolve()
        svg_output.parent.mkdir(parents=True, exist_ok=True)

    with tempfile.TemporaryDirectory(prefix="aqw_tryon_request_") as temporary:
        runtime = Path(temporary)
        assets = runtime / "assets"
        result_path = runtime / "result.json"
        shutil.copy2(host_swf, runtime / "TryOnHost.swf")
        shutil.copy2(renderer_swf, runtime / "characterB.swf")

        descriptor = (AIR_SOURCE_DIR / "TryOnHost-app.xml").read_text(
            encoding="utf-8"
        )
        application_id = f"local.aqw.tryon.r{uuid.uuid4().hex}"
        descriptor = descriptor.replace("local.aqw.tryon.request", application_id)
        (runtime / "TryOnHost-app.xml").write_text(descriptor, encoding="utf-8")

        staged = stage_assets(
            fields,
            assets,
            resolver=resolver,
            explicit_sources={override_path: override_swf},
            timeout=timeout,
        )
        request = {
            "flashvars": fields,
            "assetBase": "assets/",
            "outputPath": str(output),
            "resultPath": str(result_path),
            "captureMode": capture_mode,
            "maxSize": max_size,
            "padding": padding,
            "waitMs": round(wait_seconds * 1000),
            "timeoutMs": round((wait_seconds + 20) * 1000),
            "debug": verbose,
            "readyMode": ready_mode,
            "blankCharacter": blank_character,
            "svgOutputPath": str(svg_output) if svg_output is not None else None,
            "svgIntrinsicScale": svg_intrinsic_scale,
            "svgStrokeZoom": svg_stroke_zoom,
        }
        (runtime / "request.json").write_text(
            json.dumps(request, ensure_ascii=False),
            encoding="utf-8",
        )

        command = adl_command(air_sdk, runtime / "TryOnHost-app.xml", runtime)
        result = subprocess.run(
            command,
            cwd=runtime,
            text=True,
            capture_output=True,
            timeout=max(30.0, wait_seconds + 30.0),
            check=False,
        )
        if verbose:
            if result.stdout.strip():
                print(result.stdout.rstrip())
            if result.stderr.strip():
                print(result.stderr.rstrip(), file=sys.stderr)

        if not result_path.is_file():
            details = "\n".join(
                part.strip()
                for part in (result.stdout, result.stderr)
                if part.strip()
            )
            raise TryOnError(
                "AIR renderer exited without a result"
                + (f":\n{details}" if details else "")
            )
        render_result = json.loads(result_path.read_text(encoding="utf-8"))
        if verbose and render_result.get("diagnostics"):
            print(
                "AIR diagnostics: "
                + json.dumps(render_result["diagnostics"], sort_keys=True)
            )
        if render_result.get("status") != "ok":
            raise TryOnError(str(render_result.get("error") or "AIR render failed"))
        if not output.is_file() or output.read_bytes()[:8] != b"\x89PNG\r\n\x1a\n":
            raise TryOnError(f"AIR did not produce a valid PNG at {output}")
        if svg_output is not None:
            try:
                svg_header = svg_output.read_text(encoding="utf-8")[:512]
            except OSError as error:
                raise TryOnError(f"AIR did not produce an SVG at {svg_output}") from error
            if "<svg" not in svg_header:
                raise TryOnError(f"AIR produced an invalid SVG at {svg_output}")
        render_result["staged_assets"] = staged
        return render_result


def sanitize_filename(value: str) -> str:
    cleaned = re.sub(r"[^A-Za-z0-9_.-]+", "_", value).strip("._")
    return cleaned or "item"


def resolve_item_override(
    args: argparse.Namespace,
    fields: dict[str, str],
    resolver: LocalAssetResolver,
) -> tuple[str, Path, str, str, str | None]:
    database_slot = ""
    if args.item_id is not None:
        record = load_item(args.database, args.item_id)
        database_slot = str(record.get("slot") or "")
        slot = effective_slot(database_slot)
        if slot not in SUPPORTED_SLOTS:
            raise TryOnError(
                f"Item {args.item_id} has slot {database_slot!r}, which is not worn "
                "by the character renderer"
            )
        if args.slot is not None and args.slot != slot:
            raise TryOnError(
                f"--slot {args.slot} conflicts with item {args.item_id}'s {slot} slot"
            )
        item_name = args.name or str(record.get("name") or f"Item {args.item_id}")
        raw_file = str(record.get("file") or "")
        if slot == "armor" and "/" not in raw_file:
            remote_path = f"classes/{fields.get('strGender', 'M').upper()}/{raw_file}"
        else:
            remote_path = normalize_asset_path(raw_file)
        swf = resolver.resolve(remote_path)
        if swf is None:
            raise TryOnError(
                f"Item {args.item_id}'s SWF is not in the local archive: {remote_path}"
            )
    else:
        if args.slot is None:
            raise TryOnError("--slot is required when using --swf")
        slot = args.slot
        swf = args.swf.expanduser().resolve()
        item_name = args.name or swf.stem

    link = args.link or infer_export_link(
        swf,
        slot=slot,
        gender=fields.get("strGender", "M"),
    )
    weapon_type = None
    if slot == "weapon":
        weapon_type = args.weapon_type or infer_weapon_type(swf, database_slot)
    return slot, swf, link, item_name, weapon_type


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description=(
            "Preview one AQW item on a public character using the native AIR renderer."
        )
    )
    parser.add_argument("username", help="Public AQW character name")
    source = parser.add_mutually_exclusive_group(required=True)
    source.add_argument("--swf", type=Path, help="Local item SWF to try on")
    source.add_argument("--item-id", type=int, help="Item ID in item_db.json")
    parser.add_argument(
        "--slot",
        choices=sorted(SUPPORTED_SLOTS),
        help="Override slot (required with --swf; inferred with --item-id)",
    )
    parser.add_argument("--link", help="Exported AQW sLink (inferred when possible)")
    parser.add_argument("--name", help="Display name shown in the renderer")
    parser.add_argument(
        "--weapon-type",
        choices=("Sword", "Dagger", "Gauntlet"),
        help="Weapon placement type (normally inferred from its folder/slot)",
    )
    parser.add_argument("--output", type=Path, help="Output PNG path")
    parser.add_argument(
        "--capture",
        choices=("character", "page"),
        default="character",
        help="Capture the transparent character or full AQW character page",
    )
    parser.add_argument("--max-size", type=int, default=768)
    parser.add_argument("--padding", type=int, default=16)
    parser.add_argument(
        "--wait-seconds",
        type=float,
        default=3.0,
        help="Wait after characterB loads before capture (default: 3)",
    )
    parser.add_argument("--timeout", type=float, default=15.0)
    parser.add_argument("--asset-root", type=Path, default=DEFAULT_ASSET_ROOT)
    parser.add_argument("--database", type=Path, default=DEFAULT_DATABASE)
    parser.add_argument("--build-dir", type=Path, default=DEFAULT_BUILD_DIR)
    parser.add_argument("--air-sdk", type=Path)
    parser.add_argument("--ffdec", type=Path, help="Path to ffdec-cli.jar")
    parser.add_argument("--character-renderer", type=Path)
    parser.add_argument("--verbose", action="store_true")
    return parser


def run(args: argparse.Namespace) -> Path:
    if args.max_size < 64:
        raise TryOnError("--max-size must be at least 64")
    if args.padding < 0 or args.padding * 2 >= args.max_size:
        raise TryOnError("--padding must be nonnegative and smaller than half max size")
    if args.wait_seconds < 0:
        raise TryOnError("--wait-seconds cannot be negative")

    fields = fetch_character_flashvars(args.username, timeout=args.timeout)
    fields = {str(key): str(value) for key, value in fields.items()}
    fields["bgindex"] = "0"

    resolver = LocalAssetResolver(args.asset_root.expanduser().resolve())
    slot, swf, link, item_name, weapon_type = resolve_item_override(
        args,
        fields,
        resolver,
    )
    override_path = apply_override(
        fields,
        slot=slot,
        swf=swf,
        link=link,
        item_name=item_name,
        weapon_type=weapon_type,
    )

    output = args.output
    if output is None:
        output = DEFAULT_OUTPUT_DIR / (
            f"{sanitize_filename(fields.get('strName') or args.username)}_"
            f"{slot}_{sanitize_filename(item_name)}.png"
        )

    build_dir = args.build_dir.expanduser().resolve()
    air_sdk = resolve_air_sdk(args.air_sdk)
    ffdec = resolve_ffdec(args.ffdec)
    original_renderer = renderer_source(
        args.character_renderer,
        build_dir=build_dir,
        timeout=args.timeout,
    )
    host_swf, renderer_swf = build_native_runtime(
        build_dir=build_dir,
        air_sdk=air_sdk,
        ffdec=ffdec,
        original_renderer=original_renderer,
    )
    result = render_native(
        fields,
        override_path=override_path,
        override_swf=swf,
        resolver=resolver,
        output=output,
        host_swf=host_swf,
        renderer_swf=renderer_swf,
        air_sdk=air_sdk,
        capture_mode=args.capture,
        max_size=args.max_size,
        padding=args.padding,
        wait_seconds=args.wait_seconds,
        timeout=args.timeout,
        verbose=args.verbose,
    )

    output_path = Path(result["outputPath"])
    print(
        f"Rendered {fields.get('strName', args.username)} with {item_name} "
        f"({slot}, link={link}) -> {output_path}"
    )
    for warning in result.get("warnings", []):
        print(f"AIR warning: {warning}", file=sys.stderr)
    return output_path


def main() -> int:
    parser = build_parser()
    args = parser.parse_args()
    try:
        run(args)
    except (TryOnError, HTTPError, URLError, OSError, subprocess.SubprocessError) as error:
        print(f"Try-on failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
