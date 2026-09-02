"""Compare the assembled component SVGs between the Python reference pipeline
and the Rust worker for the synthetic raster-parity fixture (debug aid)."""

from __future__ import annotations

import subprocess
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, "services/renderer/src")

import xml.etree.ElementTree as ET

import rust_raster_parity as harness
from aqw_char_renderer import character_svg
from aqw_char_renderer.stages import component_raster

MATRIX = [
    [0.98, 0.2, -0.2, 0.98, 300.0, 140.0],
    [0.98, 0.2, -0.2, 0.98, 300.0, 150.0],
    [0.98, 0.2, -0.2, 0.98, 300.0, 160.0],
]


def python_component_svg(index: int, resources: dict) -> str:
    manifest = resources["manifest"]
    task = manifest["component_tasks"][index]
    part = manifest["parts"]["armor"]
    fields = {str(key): str(value) for key, value in manifest["fields"].items()}
    all_color_rules = [tuple(value) for value in manifest["all_color_rules"]]
    color_rules = {name: tuple(rule) for name, rule in part["color_rules"].items()}
    placement_colors = {
        tuple(int(v) for v in pair.split(",")): character_svg.AuthoredColorTransform(
            **values
        )
        for pair, values in part["placement_colors"].items()
    }
    zoom = float(manifest["settings"]["zoom"])
    viewbox = tuple(float(value) for value in manifest["viewbox"])

    with tempfile.TemporaryDirectory() as temporary:
        root_dir = Path(temporary)
        state_svg = root_dir / "state.svg"
        state_svg.write_text(harness.ffdec_frame_svg(), encoding="utf-8")
        imported = character_svg.import_ffdec_symbol(
            "armor",
            state_svg,
            zoom=zoom,
            color_rules=color_rules,
            root_class=str(part["root_class"]),
            placement_colors=placement_colors,
            root_character_id=part.get("character_id"),
        )
        element, _pixel_rect, _visible = component_raster._build_component_svg(
            imported=imported,
            matrix=tuple(float(value) for value in MATRIX[index]),
            darken=bool(task["darken"]),
            placed_key=f"{int(task['layer_index']):02d}_{task['layer_name']}",
            layer_name=str(task["layer_name"]),
            viewbox=viewbox,
            raster_size=int(manifest["settings"]["raster_size"]),
            fields=fields,
            all_color_rules=all_color_rules,
        )
        return ET.tostring(element, encoding="unicode")


def main() -> int:
    resources = harness.build_synthetic_fixture()
    rust_binary = Path(
        "services/component-raster-rust/target/release/aqw-component-raster"
    )
    store_root = Path(tempfile.mkdtemp(prefix="aqw-svg-dump-"))
    harness.populate_store(store_root, resources)
    dump_dir = Path(tempfile.mkdtemp(prefix="aqw-svg-rs-"))

    differences = 0
    for index in range(3):
        result = subprocess.run(
            [
                str(rust_binary),
                "local-raster",
                "--store-root",
                str(store_root),
                "--job-id",
                "raster-parity",
                "--task-index",
                str(index),
            ],
            capture_output=True,
            text=True,
            check=False,
            env={
                "PATH": "/usr/bin:/bin:/usr/local/bin",
                "AQW_DUMP_COMPONENT_SVG": str(dump_dir),
            },
        )
        if result.returncode:
            print(result.stderr)
            return 1
        python_svg = python_component_svg(index, resources)
        rust_svg = (dump_dir / f"task-{index:02}.svg").read_text(encoding="utf-8")
        if python_svg == rust_svg:
            print(f"task {index}: SVG identical")
        else:
            differences += 1
            print(
                f"task {index}: SVG differs ({len(python_svg)} vs {len(rust_svg)} bytes)"
            )
            out = Path(tempfile.mkdtemp(prefix="aqw-svg-diff-"))
            (out / "python.svg").write_text(python_svg, encoding="utf-8")
            (out / "rust.svg").write_text(rust_svg, encoding="utf-8")
            print(f"  written to {out}")
    return 1 if differences else 0


if __name__ == "__main__":
    raise SystemExit(main())
