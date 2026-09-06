"""Versioned messages shared by Discord, SQS, and rendering workers."""

from __future__ import annotations

import math
import re
from collections.abc import Mapping
from dataclasses import asdict, dataclass, field
from datetime import UTC, datetime
from typing import Any
from uuid import UUID

SCHEMA_VERSION = 1
MAX_USERNAME_LENGTH = 25
_USERNAME_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9 _-]*$")
_SNOWFLAKE_RE = re.compile(r"^[1-9][0-9]{0,19}$")
_APPEARANCE_KEY_RE = re.compile(r"^[A-Za-z][A-Za-z0-9]{0,63}$")
_ALLOWED_SLOTS = frozenset({"armor", "weapon", "helm", "cape", "ground"})
_MAX_APPEARANCE_FIELDS = 128
_MAX_APPEARANCE_VALUE_BYTES = 2_048
_MAX_APPEARANCE_BYTES = 32_768
_ALLOWED_RASTER_BACKENDS = frozenset({"resvg", "thorvg"})
_ALLOWED_FANOUT_MODES = frozenset({"inline", "distributed"})


class ContractError(ValueError):
    """Raised when an external job or result payload is invalid."""


def _object(value: Any, label: str) -> Mapping[str, Any]:
    if not isinstance(value, Mapping):
        raise ContractError(f"{label} must be an object")
    return value


def _only_keys(value: Mapping[str, Any], allowed: set[str], label: str) -> None:
    extras = sorted(set(value).difference(allowed))
    if extras:
        raise ContractError(f"{label} contains unsupported field(s): {', '.join(extras)}")


def normalize_username(value: Any) -> str:
    if not isinstance(value, str):
        raise ContractError("render.username must be a string")
    username = " ".join(value.strip().split())
    if not 1 <= len(username) <= MAX_USERNAME_LENGTH:
        raise ContractError(
            f"render.username must be between 1 and {MAX_USERNAME_LENGTH} characters"
        )
    if _USERNAME_RE.fullmatch(username) is None:
        raise ContractError("render.username contains unsupported characters")
    return username


def _appearance(value: Any, username: str) -> dict[str, str] | None:
    if value is None:
        return None
    payload = _object(value, "appearance")
    if len(payload) > _MAX_APPEARANCE_FIELDS:
        raise ContractError(f"appearance must contain at most {_MAX_APPEARANCE_FIELDS} fields")
    result: dict[str, str] = {}
    total_bytes = 0
    for raw_key, raw_value in payload.items():
        if not isinstance(raw_key, str) or _APPEARANCE_KEY_RE.fullmatch(raw_key) is None:
            raise ContractError(f"appearance contains an invalid field name: {raw_key!r}")
        if not isinstance(raw_value, str):
            raise ContractError(f"appearance.{raw_key} must be a string")
        value_bytes = len(raw_value.encode("utf-8"))
        if value_bytes > _MAX_APPEARANCE_VALUE_BYTES:
            raise ContractError(f"appearance.{raw_key} exceeds {_MAX_APPEARANCE_VALUE_BYTES} bytes")
        total_bytes += len(raw_key.encode("utf-8")) + value_bytes
        result[raw_key] = raw_value
    if total_bytes > _MAX_APPEARANCE_BYTES:
        raise ContractError(f"appearance exceeds {_MAX_APPEARANCE_BYTES} bytes")
    appearance_name = result.get("strName")
    if not appearance_name:
        raise ContractError("appearance.strName is required")
    if " ".join(appearance_name.strip().split()).casefold() != username.casefold():
        raise ContractError("appearance.strName does not match render.username")
    return dict(sorted(result.items()))


def _snowflake(value: Any, label: str, *, optional: bool = False) -> str | None:
    if value is None and optional:
        return None
    text = str(value)
    if _SNOWFLAKE_RE.fullmatch(text) is None:
        raise ContractError(f"{label} must be a Discord snowflake")
    return text


def _boolean(value: Any, label: str) -> bool:
    if not isinstance(value, bool):
        raise ContractError(f"{label} must be a boolean")
    return value


def _integer(value: Any, label: str, minimum: int, maximum: int) -> int:
    if isinstance(value, bool) or not isinstance(value, int):
        raise ContractError(f"{label} must be an integer")
    if not minimum <= value <= maximum:
        raise ContractError(f"{label} must be between {minimum} and {maximum}")
    return value


def _number(value: Any, label: str, minimum: float, maximum: float) -> float:
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise ContractError(f"{label} must be a number")
    number = float(value)
    if not math.isfinite(number) or not minimum <= number <= maximum:
        raise ContractError(f"{label} must be between {minimum:g} and {maximum:g}")
    return number


def _choice(value: Any, label: str, allowed: set[str]) -> str:
    text = str(value)
    if text not in allowed:
        raise ContractError(f"{label} must be one of {sorted(allowed)}")
    return text


@dataclass(frozen=True)
class DiscordTarget:
    user_id: str
    channel_id: str
    guild_id: str | None = None

    @classmethod
    def from_dict(cls, value: Any) -> DiscordTarget:
        payload = _object(value, "discord")
        _only_keys(payload, {"user_id", "guild_id", "channel_id"}, "discord")
        return cls(
            user_id=_snowflake(payload.get("user_id"), "discord.user_id") or "",
            guild_id=_snowflake(payload.get("guild_id"), "discord.guild_id", optional=True),
            channel_id=_snowflake(payload.get("channel_id"), "discord.channel_id") or "",
        )


@dataclass(frozen=True)
class ItemOverride:
    item_id: int
    slot: str | None = None

    @classmethod
    def from_dict(cls, value: Any) -> ItemOverride | None:
        if value is None:
            return None
        payload = _object(value, "render.override")
        _only_keys(payload, {"item_id", "slot"}, "render.override")
        item_id = _integer(payload.get("item_id"), "render.override.item_id", 1, 10_000_000)
        raw_slot = payload.get("slot")
        slot = None if raw_slot is None else str(raw_slot).casefold()
        if slot is not None and slot not in _ALLOWED_SLOTS:
            raise ContractError(f"render.override.slot must be one of {sorted(_ALLOWED_SLOTS)}")
        return cls(item_id=item_id, slot=slot)


@dataclass(frozen=True)
class CacheSettings:
    """Per-request permission to reuse cross-job rendering artifacts."""

    render: bool = True
    animation: bool = True
    vectors: bool = True
    bounds: bool = True
    components: bool = True

    @classmethod
    def from_dict(cls, value: Any) -> CacheSettings:
        if value is None:
            return cls()
        payload = _object(value, "cache")
        _only_keys(
            payload,
            {"render", "animation", "vectors", "bounds", "components"},
            "cache",
        )
        return cls(
            render=_boolean(payload.get("render", True), "cache.render"),
            animation=_boolean(payload.get("animation", True), "cache.animation"),
            vectors=_boolean(payload.get("vectors", True), "cache.vectors"),
            bounds=_boolean(payload.get("bounds", True), "cache.bounds"),
            components=_boolean(payload.get("components", True), "cache.components"),
        )


@dataclass(frozen=True)
class RenderSettings:
    username: str
    base_items: bool = False
    show_hidden: bool = False
    facing: str = "right"
    override: ItemOverride | None = None
    complete_loop: bool = True
    max_frames: int = 360
    subframe_start: int = 1
    zoom: float = 2.0
    raster_size: int = 2048
    output_size: int = 2048
    padding: int = 0
    output_format: str = "webp"
    avif_quality: int = 70
    avif_speed: int = 8
    webp_quality: float = 85.0
    webp_method: int = 4
    webp_lossless: bool | None = None
    # SVG rasterizer for the component pass: "resvg" (default, the pinned
    # 0.48.1 upstream) or "thorvg" (1.1.1). Both render the same assembled
    # SVG; the outputs are intentionally not pixel-identical.
    raster_backend: str = "resvg"

    @classmethod
    def from_dict(cls, value: Any) -> RenderSettings:
        payload = _object(value, "render")
        allowed = {
            "username",
            "base_items",
            "show_hidden",
            "facing",
            "override",
            "complete_loop",
            "max_frames",
            "subframe_start",
            "zoom",
            "raster_size",
            "output_size",
            # Backward-compatible alias for requests queued before v17.
            "max_size",
            "padding",
            "output_format",
            "avif_quality",
            "avif_speed",
            "webp_quality",
            "webp_method",
            "webp_lossless",
            "raster_backend",
        }
        _only_keys(payload, allowed, "render")
        facing = str(payload.get("facing", "right")).casefold()
        if facing not in {"left", "right"}:
            raise ContractError("render.facing must be left or right")
        legacy_size = payload.get("max_size")
        if legacy_size is not None and ("raster_size" in payload or "output_size" in payload):
            raise ContractError(
                "render.max_size cannot be combined with render.raster_size or render.output_size"
            )
        raster_size = _integer(
            payload.get("raster_size", legacy_size if legacy_size is not None else 2048),
            "render.raster_size",
            64,
            4096,
        )
        output_size = _integer(
            payload.get(
                "output_size",
                legacy_size if legacy_size is not None else raster_size,
            ),
            "render.output_size",
            64,
            2048,
        )
        if output_size > raster_size:
            raise ContractError("render.output_size must not exceed render.raster_size")
        padding = _integer(payload.get("padding", 0), "render.padding", 0, 1023)
        if padding * 2 >= output_size:
            raise ContractError("render.padding must be less than half render.output_size")
        return cls(
            username=normalize_username(payload.get("username")),
            base_items=_boolean(payload.get("base_items", False), "render.base_items"),
            show_hidden=_boolean(payload.get("show_hidden", False), "render.show_hidden"),
            facing=facing,
            override=ItemOverride.from_dict(payload.get("override")),
            complete_loop=_boolean(payload.get("complete_loop", True), "render.complete_loop"),
            max_frames=_integer(payload.get("max_frames", 360), "render.max_frames", 1, 2000),
            subframe_start=_integer(
                payload.get("subframe_start", 1), "render.subframe_start", 1, 10_000
            ),
            zoom=_number(payload.get("zoom", 2), "render.zoom", 0.25, 8),
            raster_size=raster_size,
            output_size=output_size,
            padding=padding,
            output_format=_choice(payload.get("output_format", "webp"), "render.output_format", {"webp", "avif"}),
            avif_quality=_integer(payload.get("avif_quality", 70), "render.avif_quality", 0, 100),
            avif_speed=_integer(payload.get("avif_speed", 8), "render.avif_speed", 0, 10),
            webp_quality=_number(payload.get("webp_quality", 85), "render.webp_quality", 0, 100),
            webp_method=_integer(payload.get("webp_method", 4), "render.webp_method", 0, 6),
            webp_lossless=(
                _boolean(payload["webp_lossless"], "render.webp_lossless")
                if payload.get("webp_lossless") is not None
                else None
            ),
            raster_backend=_choice(
                payload.get("raster_backend", "resvg"),
                "render.raster_backend",
                _ALLOWED_RASTER_BACKENDS,
            ),
        )

    def to_dict(self) -> dict[str, Any]:
        return asdict(self)


@dataclass(frozen=True)
class JobRequest:
    job_id: str
    created_at: str
    discord: DiscordTarget
    render: RenderSettings
    bounds_mode: str = "inline"
    component_raster_mode: str = "inline"
    cache: CacheSettings = field(default_factory=CacheSettings)
    appearance: dict[str, str] | None = None
    schema_version: int = field(default=SCHEMA_VERSION, init=False)

    @classmethod
    def from_dict(cls, value: Any) -> JobRequest:
        payload = _object(value, "job request")
        _only_keys(
            payload,
            {
                "schema_version",
                "job_id",
                "created_at",
                "discord",
                "render",
                "bounds_mode",
                "component_raster_mode",
                "cache",
                "appearance",
            },
            "job request",
        )
        if payload.get("schema_version") != SCHEMA_VERSION:
            raise ContractError(
                f"Unsupported schema_version {payload.get('schema_version')!r}; "
                f"expected {SCHEMA_VERSION}"
            )
        raw_job_id = payload.get("job_id")
        try:
            job_id = str(UUID(str(raw_job_id)))
        except (ValueError, TypeError, AttributeError) as error:
            raise ContractError("job_id must be a canonical UUID") from error
        if str(raw_job_id).casefold() != job_id:
            raise ContractError("job_id must be a canonical UUID")
        created_at = payload.get("created_at")
        if not isinstance(created_at, str):
            raise ContractError("created_at must be an ISO-8601 timestamp")
        try:
            timestamp = datetime.fromisoformat(created_at)
        except ValueError as error:
            raise ContractError("created_at must be an ISO-8601 timestamp") from error
        if timestamp.tzinfo is None:
            raise ContractError("created_at must include a timezone")
        normalized_time = timestamp.astimezone(UTC).isoformat().replace("+00:00", "Z")
        render = RenderSettings.from_dict(payload.get("render"))
        return cls(
            job_id=job_id,
            created_at=normalized_time,
            discord=DiscordTarget.from_dict(payload.get("discord")),
            render=render,
            bounds_mode=_choice(
                payload.get("bounds_mode", "inline"),
                "bounds_mode",
                _ALLOWED_FANOUT_MODES,
            ),
            component_raster_mode=_choice(
                payload.get("component_raster_mode", "inline"),
                "component_raster_mode",
                _ALLOWED_FANOUT_MODES,
            ),
            cache=CacheSettings.from_dict(payload.get("cache")),
            appearance=_appearance(payload.get("appearance"), render.username),
        )

    def to_dict(self) -> dict[str, Any]:
        payload = {
            "schema_version": self.schema_version,
            "job_id": self.job_id,
            "created_at": self.created_at,
            "discord": asdict(self.discord),
            "render": self.render.to_dict(),
            "bounds_mode": self.bounds_mode,
            "component_raster_mode": self.component_raster_mode,
            "appearance": self.appearance,
        }
        # Keep default requests compatible with launchers deployed before the
        # optional cache-control extension was introduced.
        if self.cache != CacheSettings():
            payload["cache"] = asdict(self.cache)
        return payload


def utc_now() -> str:
    return datetime.now(UTC).isoformat().replace("+00:00", "Z")
