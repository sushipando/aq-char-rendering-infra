"""Validated Lambda runtime configuration."""

from __future__ import annotations

import json
import os
from collections.abc import Mapping
from dataclasses import dataclass
from pathlib import Path


class ConfigurationError(RuntimeError):
    pass


def _required(environment: Mapping[str, str], name: str) -> str:
    value = environment.get(name, "").strip()
    if not value:
        raise ConfigurationError(f"Missing required environment variable {name}")
    return value


def _integer(
    environment: Mapping[str, str], name: str, default: int, minimum: int, maximum: int
) -> int:
    try:
        value = int(environment.get(name, str(default)))
    except ValueError as error:
        raise ConfigurationError(f"{name} must be an integer") from error
    if not minimum <= value <= maximum:
        raise ConfigurationError(f"{name} must be between {minimum} and {maximum}")
    return value


def _number(
    environment: Mapping[str, str], name: str, default: float, minimum: float, maximum: float
) -> float:
    try:
        value = float(environment.get(name, str(default)))
    except ValueError as error:
        raise ConfigurationError(f"{name} must be a number") from error
    if not minimum <= value <= maximum:
        raise ConfigurationError(f"{name} must be between {minimum:g} and {maximum:g}")
    return value


def _boolean(environment: Mapping[str, str], name: str, default: bool) -> bool:
    raw = environment.get(name, str(default)).strip().casefold()
    if raw in {"true", "1", "yes", "on"}:
        return True
    if raw in {"false", "0", "no", "off"}:
        return False
    raise ConfigurationError(f"{name} must be a boolean")


@dataclass(frozen=True)
class RuntimeConfig:
    source_bucket: str
    work_bucket: str
    job_table: str
    result_queue_url: str
    public_base_url: str
    asset_dataset_version: str
    asset_manifest_key: str
    character_renderer_key: str
    renderer_version: str = "v3"
    frames_per_render_lambda: int = 30
    source_bundle_frame_count: int = 30
    finalizer_download_concurrency: int = 16
    maximum_active_per_user: int = 2
    default_raster_size: int = 2048
    default_output_size: int = 2048
    default_zoom: float = 2.0
    default_padding: int = 0
    default_complete_loop: bool = True
    default_max_frames: int = 360
    default_subframe_start: int = 1
    default_webp_quality: float = 85.0
    default_webp_method: int = 4
    allow_official_asset_fallback: bool = True
    official_asset_timeout_seconds: int = 15
    # When false, prepare/finalize skip the content-addressed render cache
    # check so every job re-renders. Dev disables this to exercise the real
    # pipeline; prod enables it for cost/latency deduplication.
    render_cache_enabled: bool = True
    ffdec_path: Path = Path("/opt/ffdec/ffdec-cli.jar")
    rsvg_convert: str = "/usr/bin/rsvg-convert"
    cwebp: str = "/usr/bin/cwebp"
    webpmux: str = "/usr/bin/webpmux"

    @classmethod
    def from_env(cls, environment: Mapping[str, str] | None = None) -> RuntimeConfig:
        values = os.environ if environment is None else environment
        dataset = _required(values, "CHAR_RENDER_ASSET_DATASET_VERSION")
        legacy_batch_size = _integer(values, "CHAR_RENDER_BATCH_SIZE", 30, 1, 60)
        config = cls(
            source_bucket=_required(values, "CHAR_RENDER_SOURCE_BUCKET"),
            work_bucket=_required(values, "CHAR_RENDER_WORK_BUCKET"),
            job_table=_required(values, "CHAR_RENDER_JOB_TABLE"),
            result_queue_url=_required(values, "CHAR_RENDER_RESULT_QUEUE_URL"),
            public_base_url=_required(values, "CHAR_RENDER_PUBLIC_BASE_URL").rstrip("/"),
            asset_dataset_version=dataset,
            asset_manifest_key=values.get(
                "CHAR_RENDER_ASSET_MANIFEST_KEY", f"datasets/{dataset}/manifest.json"
            ),
            character_renderer_key=values.get(
                "CHAR_RENDER_CHARACTER_RENDERER_KEY",
                f"character-renderer/{dataset}/characterB.swf",
            ),
            renderer_version=values.get("CHAR_RENDERER_VERSION", "v3"),
            frames_per_render_lambda=_integer(
                values,
                "CHAR_RENDER_FRAMES_PER_LAMBDA",
                legacy_batch_size,
                1,
                60,
            ),
            source_bundle_frame_count=_integer(
                values,
                "CHAR_RENDER_SOURCE_BUNDLE_FRAME_COUNT",
                legacy_batch_size,
                1,
                60,
            ),
            finalizer_download_concurrency=_integer(
                values,
                "CHAR_RENDER_FINALIZER_DOWNLOAD_CONCURRENCY",
                16,
                1,
                64,
            ),
            maximum_active_per_user=_integer(values, "CHAR_RENDER_MAX_ACTIVE_PER_USER", 2, 1, 25),
            default_raster_size=_integer(
                values, "CHAR_RENDER_DEFAULT_RASTER_SIZE", 2048, 64, 4096
            ),
            default_output_size=_integer(
                values, "CHAR_RENDER_DEFAULT_OUTPUT_SIZE", 2048, 64, 2048
            ),
            default_zoom=_number(values, "CHAR_RENDER_DEFAULT_ZOOM", 2, 0.25, 8),
            default_padding=_integer(
                values, "CHAR_RENDER_DEFAULT_PADDING", 0, 0, 1023
            ),
            default_complete_loop=_boolean(
                values, "CHAR_RENDER_DEFAULT_COMPLETE_LOOP", True
            ),
            default_max_frames=_integer(
                values, "CHAR_RENDER_DEFAULT_MAX_FRAMES", 360, 1, 2000
            ),
            default_subframe_start=_integer(
                values, "CHAR_RENDER_DEFAULT_SUBFRAME_START", 1, 1, 10_000
            ),
            default_webp_quality=_number(
                values, "CHAR_RENDER_DEFAULT_WEBP_QUALITY", 85, 0, 100
            ),
            default_webp_method=_integer(
                values, "CHAR_RENDER_DEFAULT_WEBP_METHOD", 4, 0, 6
            ),
            allow_official_asset_fallback=_boolean(
                values, "CHAR_RENDER_ALLOW_OFFICIAL_ASSET_FALLBACK", True
            ),
            official_asset_timeout_seconds=_integer(
                values, "CHAR_RENDER_OFFICIAL_ASSET_TIMEOUT_SECONDS", 15, 1, 60
            ),
            render_cache_enabled=_boolean(values, "CHAR_RENDER_CACHE_ENABLED", True),
            ffdec_path=Path(values.get("CHAR_RENDER_FFDEC_PATH", "/opt/ffdec/ffdec-cli.jar")),
            rsvg_convert=values.get("CHAR_RENDER_RSVG_CONVERT", "/usr/bin/rsvg-convert"),
            cwebp=values.get("CHAR_RENDER_CWEBP", "/usr/bin/cwebp"),
            webpmux=values.get("CHAR_RENDER_WEBPMUX", "/usr/bin/webpmux"),
        )
        if config.default_output_size > config.default_raster_size:
            raise ConfigurationError(
                "CHAR_RENDER_DEFAULT_OUTPUT_SIZE must not exceed "
                "CHAR_RENDER_DEFAULT_RASTER_SIZE"
            )
        if config.default_padding * 2 >= config.default_output_size:
            raise ConfigurationError(
                "CHAR_RENDER_DEFAULT_PADDING must be less than half "
                "CHAR_RENDER_DEFAULT_OUTPUT_SIZE"
            )
        return config

    def worker_concurrency(self, environment: Mapping[str, str] | None = None) -> dict[str, int]:
        values = os.environ if environment is None else environment
        raw = values.get("CHAR_RENDER_WORKER_CONCURRENCY", "{}")
        try:
            parsed = json.loads(raw)
        except json.JSONDecodeError as error:
            raise ConfigurationError("CHAR_RENDER_WORKER_CONCURRENCY must be JSON") from error
        if not isinstance(parsed, dict):
            raise ConfigurationError("CHAR_RENDER_WORKER_CONCURRENCY must be an object")
        return {str(key): int(value) for key, value in parsed.items()}

    def render_defaults(self) -> dict[str, object]:
        """Defaults injected into sparse public requests by the launcher."""
        return {
            "complete_loop": self.default_complete_loop,
            "max_frames": self.default_max_frames,
            "subframe_start": self.default_subframe_start,
            "zoom": self.default_zoom,
            "raster_size": self.default_raster_size,
            "output_size": self.default_output_size,
            "padding": self.default_padding,
            "webp_quality": self.default_webp_quality,
            "webp_method": self.default_webp_method,
        }
