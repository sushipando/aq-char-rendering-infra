"""Compose a deterministic batch of complete character SVG frames."""

from __future__ import annotations

import tempfile
from pathlib import Path
from typing import Any, Protocol

from aqw_char_renderer import character_svg
from aqw_char_renderer.config import RuntimeConfig
from aqw_char_renderer.legacy import render_swf_items as item_renderer


class StageStore(Protocol):
    def download(self, bucket: str, key: str, destination: Path, **kwargs: Any) -> Path: ...
    def upload_file(self, source: Path, bucket: str, key: str, **kwargs: Any) -> None: ...
    def read_json(self, bucket: str, key: str) -> Any: ...
    def write_json(self, bucket: str, key: str, value: Any) -> None: ...


def compose_batch(
    *,
    job_id: str,
    manifest_key: str,
    batch: dict[str, int],
    store: StageStore,
    config: RuntimeConfig,
) -> dict[str, Any]:
    manifest = store.read_json(config.work_bucket, manifest_key)
    if manifest.get("job_id") != job_id:
        raise character_svg.CharacterSvgError("Prepare manifest belongs to another job")
    frame_start = int(batch["frame_start"])
    frame_end = int(batch["frame_end"])
    batch_index = int(batch["index"])
    layers = character_svg.build_layers(manifest["aliases"], weapon_type=manifest["weapon_type"])
    records: list[dict[str, Any]] = []
    all_warnings: list[str] = []
    with tempfile.TemporaryDirectory(prefix=f"aqw-compose-{job_id}-{batch_index}-") as temporary:
        root = Path(temporary)
        for frame_number in range(frame_start, frame_end + 1):
            imported: dict[str, character_svg.ImportedSymbol] = {}
            for key, part in manifest["parts"].items():
                raw_key = part["frames"][frame_number - 1]
                raw_path = store.download(
                    config.work_bucket,
                    raw_key,
                    root / "raw" / key / f"{frame_number:06d}.svg",
                )
                imported[key] = character_svg.import_ffdec_symbol(
                    key,
                    raw_path,
                    zoom=float(manifest["settings"]["zoom"]),
                    color_rules={name: tuple(rule) for name, rule in part["color_rules"].items()},
                    root_class=part["root_class"],
                )
            output = root / "svg" / f"{frame_number:06d}.svg"
            warnings = character_svg.compose_svg(
                imported,
                layers,
                fields=manifest["fields"],
                all_color_rules=[tuple(value) for value in manifest["all_color_rules"]],
                output=output,
                max_size=int(manifest["settings"]["max_size"]),
                padding=0,
                facing=manifest["settings"]["facing"],
                rsvg_convert=config.rsvg_convert,
            )
            bounds = item_renderer.svg_canvas_viewbox(output)
            if bounds is None:
                raise character_svg.CharacterSvgError(
                    f"Composed frame {frame_number} has no usable bounds"
                )
            output_key = f"jobs/{job_id}/svg/{frame_number:06d}.svg"
            store.upload_file(output, config.work_bucket, output_key, content_type="image/svg+xml")
            records.append(
                {
                    "frame": frame_number,
                    "svg_key": output_key,
                    "bounds": list(bounds),
                    "warnings": warnings,
                }
            )
            all_warnings.extend(warning for warning in warnings if warning not in all_warnings)
    batch_manifest_key = f"jobs/{job_id}/compose/batch-{batch_index:04d}.json"
    store.write_json(
        config.work_bucket,
        batch_manifest_key,
        {
            "schema_version": 1,
            "job_id": job_id,
            "batch": batch_index,
            "frames": records,
            "warnings": all_warnings,
        },
    )
    return {
        "job_id": job_id,
        "batch": batch_index,
        "batch_manifest_key": batch_manifest_key,
    }
