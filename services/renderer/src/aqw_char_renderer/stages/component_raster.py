"""Rasterize one unique placed component state to a tight transparent PNG.

Each task covers one unique **placed component state**: one raw SVG state
combined with one exact character placement (layer name, complete characterB
matrix, facing, display scale, front/back darkening) and the job's already
decided shared viewbox/pixel scale. All scale, rotation, mirroring, skew, and
translation are baked into a single-layer SVG before its one rasterization;
no affine transform is ever applied to the PNG.

The component SVG uses a tight page at the exact job raster pixel scale
instead of rasterizing a full transparent character canvas. Existing
minimum-stroke calibration therefore still happens at the full raster size.
After rasterization, the finished PNG is shrunk once with premultiplied-alpha
Lanczos filtering onto the exact final output grid. The compositor only
places already-transformed, already-sized PNGs at integer output offsets.
"""

from __future__ import annotations

import math
import tarfile
import tempfile
import time
import xml.etree.ElementTree as ET
from collections.abc import Mapping
from pathlib import Path
from typing import Any, Protocol

from PIL import Image

from aqw_char_renderer import character_svg
from aqw_char_renderer.config import RuntimeConfig
from aqw_char_renderer.hashing import file_sha256
from aqw_char_renderer.legacy import render_swf_items as item_renderer
from aqw_char_renderer.stages.compose_frames import _frame_canvas_sizes
from aqw_char_renderer.structured_logging import log_event

# Raster padding around a component's vector bounds, in raster pixels, so
# authored filter glow that spills past the artwork's tight bounds is not
# clipped per component (the final frame canvas still clips at its edges,
# matching the composed-frame pipeline's accepted glow trade-off). The page
# is cropped back to visible alpha afterward, so this only costs raster area.
PAGE_MARGIN_PIXELS = 24

# Lanczos3 needs six source pixels of support for the configured 2x shrink.
# Keep twice that much transparent room around each visible component so a
# tight component page produces the same filter result as a full transparent
# character canvas, including at fractional output-grid phases.
DOWNSAMPLE_HALO_RASTER_PIXELS = 12

COMPONENT_RASTER_SPACE_RASTER = "raster"
COMPONENT_RASTER_SPACE_OUTPUT = "output"


class StageStore(Protocol):
    def download(self, bucket: str, key: str, destination: Path, **kwargs: Any) -> Path: ...
    def upload_file(self, source: Path, bucket: str, key: str, **kwargs: Any) -> None: ...
    def write_json(self, bucket: str, key: str, value: Any) -> None: ...
    def read_json(self, bucket: str, key: str) -> Any: ...


def _extract_member(archive_path: Path, member: str, destination: Path) -> Path:
    destination.parent.mkdir(parents=True, exist_ok=True)
    with tarfile.open(archive_path) as archive:
        info = archive.getmember(member)
        source = archive.extractfile(info)
        if source is None:
            raise character_svg.CharacterSvgError(
                f"Component bundle has no readable member {member}"
            )
        with destination.open("wb") as output:
            while chunk := source.read(1024 * 1024):
                output.write(chunk)
    return destination


def _build_component_svg(
    *,
    imported: character_svg.ImportedSymbol,
    matrix: tuple[float, float, float, float, float, float],
    darken: bool,
    placed_key: str,
    layer_name: str,
    viewbox: tuple[float, float, float, float],
    raster_size: int,
    fields: dict[str, str],
    all_color_rules: list[tuple[str, str]],
) -> tuple[ET.Element, tuple[int, int, int, int], bool]:
    """Compose one component's single-layer SVG with a shared-scale tight page.

    Returns the root element, the integer pixel rect ``(left, top, right,
    bottom)`` of the page on the shared raster canvas, and whether the state
    has any drawable extent. Zero-size states (authored invisible frames)
    produce an empty page.
    """
    root = ET.Element(
        f"{{{character_svg.SVG_NS}}}svg",
        {
            "version": "1.1",
            f"{{{character_svg.FFDEC_NS}}}objectType": "aqw-component",
            "data-renderer": "ffdec-component-raster-v1",
        },
    )
    defs = ET.SubElement(root, f"{{{character_svg.SVG_NS}}}defs")
    character_svg.add_color_filters(defs, all_color_rules, fields)

    symbol = character_svg.clone_imported_symbol(imported, placed_key)
    prepared, _malformed = character_svg.prepare_minimum_strokes(
        symbol,
        layer_scale=item_renderer.affine_geometric_scale(matrix),
    )
    if prepared:
        root.set("data-aqw-calibrated-minimum-strokes", str(prepared))
    for definition in symbol.definitions:
        defs.append(definition)
    defs.append(symbol.definition)

    group = ET.SubElement(root, f"{{{character_svg.SVG_NS}}}g", {"id": "aqw-component"})
    attributes = {
        "id": f"layer-{layer_name}",
        f"{{{character_svg.XLINK_NS}}}href": f"#symbol_{placed_key}",
        "href": f"#symbol_{placed_key}",
        "transform": character_svg.matrix_text(matrix),
    }
    if darken:
        attributes["filter"] = "url(#aqw_back_part_dark)"
    ET.SubElement(group, f"{{{character_svg.SVG_NS}}}use", attributes)

    bounds = character_svg._transformed_bounds(imported.bounds, matrix)
    x, y, width, height = bounds
    if width <= 0 or height <= 0:
        return root, (0, 0, 0, 0), False

    viewbox_x, viewbox_y, viewbox_width, viewbox_height = viewbox
    pixel_scale = raster_size / max(viewbox_width, viewbox_height)
    margin = PAGE_MARGIN_PIXELS
    # Integer pixel edges on the shared canvas: the page is an exact sub-rect
    # of the final composition canvas at the job's one pixel scale.
    left = math.floor((x - viewbox_x) * pixel_scale) - margin
    top = math.floor((y - viewbox_y) * pixel_scale) - margin
    right = math.ceil((x + width - viewbox_x) * pixel_scale) + margin
    bottom = math.ceil((y + height - viewbox_y) * pixel_scale) + margin
    page_width = max(1, right - left)
    page_height = max(1, bottom - top)
    page_x = viewbox_x + left / pixel_scale
    page_y = viewbox_y + top / pixel_scale
    root.set(
        "viewBox",
        f"{page_x:.9g} {page_y:.9g} {page_width / pixel_scale:.9g} {page_height / pixel_scale:.9g}",
    )
    root.set("width", f"{page_width}px")
    root.set("height", f"{page_height}px")
    character_svg.calibrate_minimum_strokes(root)
    return root, (left, top, right, bottom), True


def _downsample_component_to_output_grid(
    image: Image.Image,
    *,
    x: int,
    y: int,
    raster_canvas: tuple[int, int],
    output_canvas: tuple[int, int],
    halo: int = DOWNSAMPLE_HALO_RASTER_PIXELS,
) -> tuple[Image.Image, int, int] | None:
    """Shrink one cropped layer on the exact full-frame output pixel grid.

    ``Image.resize(..., box=...)`` preserves the sampling phase of resizing a
    complete transparent component canvas, even when an aspect-ratio
    dimension uses a non-integer scale such as 697 -> 348. The source box is
    expanded into transparent pixels before filtering so the tight crop does
    not clamp Lanczos at the artwork edge.
    """
    if image.mode != "RGBA":
        raise character_svg.CharacterSvgError("Component downsample input is not RGBA")
    if halo < 0:
        raise character_svg.CharacterSvgError("Component downsample halo is negative")
    source_width, source_height = raster_canvas
    target_width, target_height = output_canvas
    if min(source_width, source_height, target_width, target_height) <= 0:
        raise character_svg.CharacterSvgError("Component canvas dimensions are invalid")
    if target_width > source_width or target_height > source_height:
        raise character_svg.CharacterSvgError(
            "Component output canvas cannot exceed its raster canvas"
        )

    visible_left = max(0, x)
    visible_top = max(0, y)
    visible_right = min(source_width, x + image.width)
    visible_bottom = min(source_height, y + image.height)
    if visible_left >= visible_right or visible_top >= visible_bottom:
        return None

    # Select an integer output rect around the component. Its corresponding
    # source box can be fractional; using that exact box is what retains the
    # global full-frame resampling phase.
    output_left = max(
        0,
        math.floor((visible_left - halo) * target_width / source_width),
    )
    output_top = max(
        0,
        math.floor((visible_top - halo) * target_height / source_height),
    )
    output_right = min(
        target_width,
        math.ceil((visible_right + halo) * target_width / source_width),
    )
    output_bottom = min(
        target_height,
        math.ceil((visible_bottom + halo) * target_height / source_height),
    )
    if output_left >= output_right or output_top >= output_bottom:
        return None

    box_left = output_left * source_width / target_width
    box_top = output_top * source_height / target_height
    box_right = output_right * source_width / target_width
    box_bottom = output_bottom * source_height / target_height
    patch_left = max(0, math.floor(box_left))
    patch_top = max(0, math.floor(box_top))
    patch_right = min(source_width, math.ceil(box_right))
    patch_bottom = min(source_height, math.ceil(box_bottom))

    patch = Image.new(
        "RGBA",
        (patch_right - patch_left, patch_bottom - patch_top),
        (0, 0, 0, 0),
    )
    patch.alpha_composite(image, dest=(x - patch_left, y - patch_top))
    premultiplied = patch.convert("RGBa")
    patch.close()
    try:
        resized = premultiplied.resize(
            (output_right - output_left, output_bottom - output_top),
            Image.Resampling.LANCZOS,
            box=(
                box_left - patch_left,
                box_top - patch_top,
                box_right - patch_left,
                box_bottom - patch_top,
            ),
            reducing_gap=3.0,
        )
        try:
            converted = resized.convert("RGBA")
        finally:
            resized.close()
    finally:
        premultiplied.close()

    bbox = item_renderer.alpha_bbox(converted)
    if bbox is None:
        converted.close()
        return None
    crop_left, crop_top, _crop_right, _crop_bottom = bbox
    cropped = converted.crop(bbox)
    converted.close()
    return cropped, output_left + crop_left, output_top + crop_top


def rasterize_component_state(
    *,
    job_id: str,
    manifest_key: str,
    task: dict[str, Any] | None = None,
    task_index: int | None = None,
    store: StageStore,
    config: RuntimeConfig,
) -> dict[str, Any]:
    """Rasterize exactly one unique placed component state.

    The task identity (``task["task_id"]``) is deterministic and its result
    record and PNG use deterministic S3 keys, so retries are idempotent.
    """
    started = time.perf_counter()
    timings: dict[str, float] = {
        "manifest_ms": 0.0,
        "bundle_download_ms": 0.0,
        "extract_ms": 0.0,
        "import_ms": 0.0,
        "svg_build_ms": 0.0,
        "rasterize_ms": 0.0,
        "crop_ms": 0.0,
        "downsample_ms": 0.0,
        "upload_ms": 0.0,
    }

    manifest_started = time.perf_counter()
    prepared = store.read_json(config.work_bucket, manifest_key)
    timings["manifest_ms"] = (time.perf_counter() - manifest_started) * 1000
    if prepared.get("job_id") != job_id:
        raise character_svg.CharacterSvgError("Prepare manifest belongs to another job")
    if task is None:
        component_tasks = prepared.get("component_tasks")
        if not isinstance(component_tasks, list) or task_index is None:
            raise character_svg.CharacterSvgError("Component raster request has no usable task")
        if task_index < 0 or task_index >= len(component_tasks):
            raise character_svg.CharacterSvgError(
                f"Component task index {task_index} is out of range"
            )
        selected = component_tasks[task_index]
        if not isinstance(selected, dict):
            raise character_svg.CharacterSvgError(f"Component task index {task_index} is malformed")
        task = selected

    viewbox = tuple(float(value) for value in prepared["viewbox"])
    if len(viewbox) != 4:
        raise character_svg.CharacterSvgError("Prepare manifest has no usable shared viewbox")
    settings = prepared["settings"]
    raster_size = int(settings["raster_size"])
    output_size = int(settings["output_size"])
    raster_canvas, output_canvas = _frame_canvas_sizes(
        viewbox,
        raster_size=raster_size,
        output_size=output_size,
    )
    component_raster_space = str(
        prepared.get("component_raster_space") or COMPONENT_RASTER_SPACE_RASTER
    )
    if component_raster_space not in {
        COMPONENT_RASTER_SPACE_RASTER,
        COMPONENT_RASTER_SPACE_OUTPUT,
    }:
        raise character_svg.CharacterSvgError(
            f"Unsupported component raster space {component_raster_space!r}"
        )
    zoom = float(settings["zoom"])
    fields = {str(key): str(value) for key, value in prepared["fields"].items()}
    all_color_rules = [tuple(value) for value in prepared["all_color_rules"]]
    symbol_key = str(task["symbol_key"])
    part = prepared["parts"].get(symbol_key)
    if part is None:
        raise character_svg.CharacterSvgError(
            f"Component task references unknown part {symbol_key}"
        )
    color_rules = {name: tuple(rule) for name, rule in part.get("color_rules", {}).items()}
    placement_colors = {
        tuple(
            int(component) for component in pair.split(",")
        ): character_svg.AuthoredColorTransform(**values)
        for pair, values in part.get("placement_colors", {}).items()
    }
    matrix = tuple(float(value) for value in task["matrix"])
    darken = bool(task.get("darken") or False)
    bundle_key = str(task["bundle_key"])
    member = str(task["member"])
    task_id = str(task["task_id"])

    with tempfile.TemporaryDirectory(
        prefix=f"aqw-component-{job_id[:8]}-{task_id[:8]}-"
    ) as temporary:
        root = Path(temporary)
        bundle_started = time.perf_counter()
        archive_path = store.download(
            config.work_bucket,
            bundle_key,
            root / "bundle.tar.gz",
        )
        timings["bundle_download_ms"] = (time.perf_counter() - bundle_started) * 1000
        extract_started = time.perf_counter()
        raw_state = _extract_member(archive_path, member, root / "state.svg")
        timings["extract_ms"] = (time.perf_counter() - extract_started) * 1000
        input_bytes = raw_state.stat().st_size

        import_started = time.perf_counter()
        imported = character_svg.import_ffdec_symbol(
            symbol_key,
            raw_state,
            zoom=zoom,
            color_rules=color_rules,
            root_class=str(part["root_class"]),
            placement_colors=placement_colors,
            root_character_id=part.get("character_id"),
        )
        timings["import_ms"] = (time.perf_counter() - import_started) * 1000

        svg_started = time.perf_counter()
        placed_key = f"{int(task['layer_index']):02d}_{task['layer_name']}"
        element, pixel_rect, visible = _build_component_svg(
            imported=imported,
            matrix=matrix,
            darken=darken,
            placed_key=placed_key,
            layer_name=str(task["layer_name"]),
            viewbox=viewbox,
            raster_size=raster_size,
            fields=fields,
            all_color_rules=all_color_rules,
        )
        filter_count = sum(
            1
            for element in element.iter()
            if element.tag.rsplit("}", 1)[-1] in {"filter", "feGaussianBlur"}
        )
        component_svg = root / "component.svg"
        tree = ET.ElementTree(element)
        tree.write(component_svg, encoding="utf-8", xml_declaration=True)
        svg_bytes = component_svg.stat().st_size
        timings["svg_build_ms"] = (time.perf_counter() - svg_started) * 1000

        png = root / "component.png"
        left, top, right, bottom = pixel_rect
        page_width = max(1, right - left)
        page_height = max(1, bottom - top)
        record: dict[str, Any] = {
            "task_id": task_id,
            "empty": True,
            "x": 0,
            "y": 0,
            "width": 0,
            "height": 0,
        }
        if visible:
            rasterize_started = time.perf_counter()
            rendered = item_renderer.render_svg_to_maximum(
                component_svg,
                png,
                maximum=max(page_width, page_height),
                rsvg_convert=config.rsvg_convert,
            )
            timings["rasterize_ms"] = (time.perf_counter() - rasterize_started) * 1000
            if rendered is None:
                raise character_svg.CharacterSvgError(f"Unable to rasterize component {task_id}")
            if tuple(rendered) != (page_width, page_height):
                raise character_svg.CharacterSvgError(
                    f"Component {task_id} rendered {rendered}, expected "
                    f"{page_width}x{page_height} (shared pixel scale mismatch)"
                )
            crop_started = time.perf_counter()
            with Image.open(png) as image:
                if image.mode != "RGBA":
                    raise character_svg.CharacterSvgError(
                        f"Component {task_id} is not transparent RGBA"
                    )
                bbox = item_renderer.alpha_bbox(image)
                if bbox is None:
                    timings["crop_ms"] = (time.perf_counter() - crop_started) * 1000
                else:
                    crop_left, crop_top, _crop_right, _crop_bottom = bbox
                    cropped = image.crop(bbox)
                    component_x = left + crop_left
                    component_y = top + crop_top
                    timings["crop_ms"] = (time.perf_counter() - crop_started) * 1000
                    try:
                        if (
                            component_raster_space == COMPONENT_RASTER_SPACE_OUTPUT
                            and output_canvas != raster_canvas
                        ):
                            downsample_started = time.perf_counter()
                            scaled = _downsample_component_to_output_grid(
                                cropped,
                                x=component_x,
                                y=component_y,
                                raster_canvas=raster_canvas,
                                output_canvas=output_canvas,
                            )
                            timings["downsample_ms"] = (
                                time.perf_counter() - downsample_started
                            ) * 1000
                            if scaled is not None:
                                output_image, component_x, component_y = scaled
                            else:
                                output_image = None
                        else:
                            output_image = cropped

                        if output_image is not None:
                            try:
                                output_image.save(png, format="PNG")
                                record = {
                                    "task_id": task_id,
                                    "empty": False,
                                    "x": component_x,
                                    "y": component_y,
                                    "width": output_image.width,
                                    "height": output_image.height,
                                    "sha256": file_sha256(png),
                                    "bytes": png.stat().st_size,
                                }
                            finally:
                                if output_image is not cropped:
                                    output_image.close()
                    finally:
                        cropped.close()

        png_key = f"jobs/{job_id}/component/rasters/{task_id}.png"
        result_key = f"jobs/{job_id}/component/results/{task_id}.json"
        upload_started = time.perf_counter()
        if record.get("empty") is not True:
            store.upload_file(png, config.work_bucket, png_key, content_type="image/png")
        timings["upload_ms"] = (time.perf_counter() - upload_started) * 1000

    record.update(
        {
            "png_key": png_key if record.get("empty") is not True else None,
            "input_bytes": input_bytes,
            "svg_bytes": svg_bytes,
            "filter_count": filter_count,
            "component_raster_space": component_raster_space,
            "canvas_width": (
                output_canvas[0]
                if component_raster_space == COMPONENT_RASTER_SPACE_OUTPUT
                else raster_canvas[0]
            ),
            "canvas_height": (
                output_canvas[1]
                if component_raster_space == COMPONENT_RASTER_SPACE_OUTPUT
                else raster_canvas[1]
            ),
            "state_signature": task.get("state_signature"),
            "symbol_key": symbol_key,
            "layer_name": task.get("layer_name"),
        }
    )
    store.write_json(config.work_bucket, result_key, record)
    record["result_key"] = result_key
    record["rasterize_ms"] = round(timings["rasterize_ms"], 2)
    record["crop_ms"] = round(timings["crop_ms"], 2)
    record["downsample_ms"] = round(timings["downsample_ms"], 2)
    total_ms = (time.perf_counter() - started) * 1000
    accounted = sum(timings.values())
    log_event(
        "component_raster_profile",
        job_id=job_id,
        task_id=task_id,
        symbol_key=symbol_key,
        layer_name=task.get("layer_name"),
        empty=record.get("empty", False),
        svg_bytes=svg_bytes,
        input_bytes=input_bytes,
        filter_count=filter_count,
        raster_pixel_count=(
            record["width"] * record["height"] if record.get("empty") is not True else 0
        ),
        component_raster_space=component_raster_space,
        raster_canvas_width=raster_canvas[0],
        raster_canvas_height=raster_canvas[1],
        output_canvas_width=output_canvas[0],
        output_canvas_height=output_canvas[1],
        total_ms=round(total_ms, 1),
        unaccounted_ms=round(total_ms - accounted, 1),
        **{key: round(value, 1) for key, value in timings.items()},
    )
    return record


def component_workflow_result(record: Mapping[str, Any]) -> dict[str, Any]:
    """Return only fields that the frame compositor needs in workflow state.

    The complete diagnostic record remains in S3. Keeping it out of the
    Inline Map result prevents otherwise valid characters from approaching
    Step Functions' 256 KiB execution-state limit.
    """
    compact: dict[str, Any] = {
        "task_id": str(record["task_id"]),
        "empty": bool(record.get("empty") is True),
    }
    if compact["empty"]:
        return compact
    compact.update(
        {
            "png_key": str(record["png_key"]),
            "sha256": str(record["sha256"]),
            "x": int(record["x"]),
            "y": int(record["y"]),
            "component_raster_space": str(record["component_raster_space"]),
        }
    )
    return compact
