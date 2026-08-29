from __future__ import annotations

import json
from pathlib import Path
import struct
import tempfile
import unittest
from urllib.error import HTTPError
from unittest import mock
import xml.etree.ElementTree as ET

from PIL import Image

from aqw_char_renderer import character_svg
from aqw_char_renderer.legacy import render_swf_items as item_renderer


class RenderSwfCharacterSvgTests(unittest.TestCase):
    def assertMatrixAlmostEqual(self, actual, expected, places=8):
        self.assertEqual(len(actual), len(expected))
        for actual_value, expected_value in zip(actual, expected):
            self.assertAlmostEqual(actual_value, expected_value, places=places)

    def test_appearance_uses_cosmetics_and_respects_visibility(self):
        fields = {
            "strGender": "F",
            "ia1": "1",  # cape hidden, helm visible
            "strClassFile": "Base.swf",
            "strClassLink": "Base",
            "strCustArmorName": "Cosmetic armor",
            "strCustArmorFile": "Cosmetic.swf",
            "strCustArmorLink": "Cosmetic",
            "strWeaponFile": "items/swords/Base.swf",
            "strWeaponLink": "BaseWeapon",
            "strWeaponType": "Sword",
            "strCustWeaponName": "Cosmetic weapon",
            "strCustWeaponFile": "items/daggers/Cosmetic.swf",
            "strCustWeaponLink": "CosmeticWeapon",
            "strCustWeaponType": "Dagger",
            "strHelmFile": "items/helms/Helm.swf",
            "strHelmLink": "Helm",
            "strCapeFile": "items/capes/Cape.swf",
            "strCapeLink": "Cape",
            "strMiscFile": "items/grounds/Ground.swf",
            "strMiscLink": "Ground",
        }

        cosmetic = character_svg.appearance_assets(fields)
        base = character_svg.appearance_assets(fields, use_cosmetics=False)

        self.assertEqual(cosmetic["armor"].remote_path, "classes/F/Cosmetic.swf")
        self.assertEqual(cosmetic["weapon"].weapon_type, "Dagger")
        self.assertEqual(cosmetic["helm"].link, "Helm")
        self.assertNotIn("cape", cosmetic)
        self.assertEqual(cosmetic["ground"].link, "Ground")
        self.assertEqual(base["armor"].remote_path, "classes/F/Base.swf")
        self.assertEqual(base["weapon"].weapon_type, "Sword")

    def test_hidden_helm_uses_hair_but_blank_hair_is_omitted(self):
        fields = {
            "strGender": "M",
            "ia1": "2",
            "strClassFile": "Armor.swf",
            "strClassLink": "Armor",
            "strHelmFile": "items/helms/Helm.swf",
            "strHelmLink": "Helm",
            "strHairFile": "hair/M/Style.swf",
            "strHairName": "Style",
        }
        assets = character_svg.appearance_assets(fields)
        self.assertNotIn("helm", assets)
        self.assertEqual(assets["hair"].link, "StyleMHair")

        fields["strHairName"] = "Blank"
        self.assertNotIn("hair", character_svg.appearance_assets(fields))

    def test_character_page_403_uses_official_flashvars_fallback(self):
        fallback = "&strName=Artix&strGender=M"
        calls = []

        def fake_fetch(url, *, timeout):
            calls.append((url, timeout))
            if url.startswith(character_svg.tryon.CHARACTER_PAGE_URL):
                raise HTTPError(url, 403, "Forbidden", None, None)
            return fallback

        with mock.patch.object(character_svg.tryon, "fetch_text", side_effect=fake_fetch):
            fields = character_svg.tryon.fetch_character_flashvars("artix", timeout=9)

        self.assertEqual(fields["strName"], "Artix")
        self.assertEqual(fields["strGender"], "M")
        self.assertEqual(len(calls), 2)
        self.assertTrue(calls[1][0].startswith(character_svg.tryon.FALLBACK_FVARS_URL))

    def test_empty_legacy_link_uses_compiled_root_sprite(self):
        with tempfile.TemporaryDirectory() as temporary:
            source = Path(temporary) / "axe05.swf"
            source.write_bytes(b"FWS legacy")
            with (
                mock.patch.object(character_svg.tryon, "symbol_class_entries", return_value=[]),
                mock.patch.object(
                    item_renderer,
                    "parse_swf_sprite_metadata",
                    return_value={"root": 12},
                ),
            ):
                self.assertEqual(character_svg.symbol_id(source, ""), (12, "axe05"))

    def test_dagger_offhand_is_immediately_in_front_of_cape(self):
        aliases = {
            "cape": "cape",
            "weapon": "weapon",
            "shoulder": "shoulder",
            "hand": "hand",
            "chest": "chest",
            "hip": "hip",
            "thigh": "thigh",
            "shin": "shin",
            "idle_foot": "idle",
            "back_foot": "foot",
            "head": "head",
        }
        layers = character_svg.build_layers(aliases, weapon_type="Dagger")
        names = [layer.name for layer in layers]

        self.assertEqual(
            names[names.index("cape") : names.index("cape") + 3],
            ["cape", "weapon_off", "back_shoulder"],
        )
        self.assertLess(names.index("weapon"), names.index("front_shoulder"))
        self.assertLess(names.index("front_shoulder"), names.index("front_hand"))

    def test_gauntlet_is_inserted_in_both_hand_holders(self):
        aliases = {"weapon": "weapon", "hand": "hand"}
        names = [
            layer.name
            for layer in character_svg.build_layers(aliases, weapon_type="Gauntlet")
        ]
        self.assertNotIn("weapon", names)
        self.assertEqual(
            names,
            ["back_hand", "gauntlet_back", "front_hand", "gauntlet_front"],
        )

    def test_import_removes_ffdec_crop_and_zoom_matrix(self):
        with tempfile.TemporaryDirectory() as temporary:
            source = Path(temporary) / "part.svg"
            source.write_text(
                """<?xml version="1.0"?>
                <svg xmlns="http://www.w3.org/2000/svg"
                     xmlns:xlink="http://www.w3.org/1999/xlink"
                     width="20px" height="10px">
                  <g transform="matrix(2,0,0,2,6,8)">
                    <use xlink:href="#shape0"/>
                  </g>
                  <defs><g id="shape0"><rect width="2" height="3"/></g></defs>
                </svg>""",
                encoding="utf-8",
            )

            imported = character_svg.import_ffdec_symbol(
                "test",
                source,
                zoom=2,
                color_rules={},
                root_class="Test",
            )

            self.assertEqual(imported.bounds, (-3.0, -4.0, 10.0, 5.0))
            self.assertEqual(imported.export_zoom, 2)
            self.assertEqual(imported.minimum_stroke_scale, 0.5)
            frame = next(iter(imported.definition))
            self.assertIsNone(frame.get("transform"))
            self.assertEqual(imported.definition.get("id"), "symbol_test")
            self.assertTrue(imported.definitions[0].get("id", "").startswith("part_test_"))

    def test_import_accepts_an_intentionally_empty_symbol(self):
        with tempfile.TemporaryDirectory() as temporary:
            source = Path(temporary) / "empty.svg"
            source.write_text(
                '<svg xmlns="http://www.w3.org/2000/svg" width="0px" height="0px"/>',
                encoding="utf-8",
            )
            imported = character_svg.import_ffdec_symbol(
                "unarmed",
                source,
                zoom=1,
                color_rules={},
                root_class="unarmed",
            )
            self.assertEqual(imported.bounds, (0.0, 0.0, 0.0, 0.0))
            self.assertEqual(imported.definition.get("id"), "symbol_unarmed")

    def test_import_restores_nested_authored_color_transform(self):
        with tempfile.TemporaryDirectory() as temporary:
            source = Path(temporary) / "part.svg"
            source.write_text(
                """<?xml version="1.0"?>
                <svg xmlns="http://www.w3.org/2000/svg"
                     xmlns:xlink="http://www.w3.org/1999/xlink"
                     xmlns:ffdec="https://www.free-decompiler.com/flash"
                     width="20px" height="10px">
                  <g transform="matrix(1,0,0,1,0,0)">
                    <use ffdec:characterId="31" xlink:href="#sprite0"/>
                  </g>
                  <defs>
                    <g id="sprite0">
                      <use ffdec:characterId="19" xlink:href="#shape0"/>
                    </g>
                    <g id="shape0"><rect width="2" height="3"/></g>
                  </defs>
                </svg>""",
                encoding="utf-8",
            )
            black = character_svg.AuthoredColorTransform(
                red_mult=0,
                green_mult=0,
                blue_mult=0,
            )

            imported = character_svg.import_ffdec_symbol(
                "armor",
                source,
                zoom=1,
                color_rules={},
                root_class="Armor",
                placement_colors={(31, 19): black},
                root_character_id=32,
            )

            root = ET.Element("root")
            root.extend(imported.definitions)
            root.append(imported.definition)
            authored_filters = [
                element
                for element in root.iter()
                if element.tag.endswith("filter")
                and "authored_cxform" in (element.get("id") or "")
            ]
            self.assertEqual(len(authored_filters), 1)
            self.assertEqual(
                next(iter(authored_filters[0])).get("values"),
                "0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 1 0",
            )
            wrappers = [
                element
                for element in root.iter()
                if "authored_cxform" in element.get("filter", "")
            ]
            self.assertEqual(len(wrappers), 1)
            self.assertEqual(
                next(iter(wrappers[0])).get(
                    f"{{{character_svg.FFDEC_NS}}}characterId"
                ),
                "19",
            )

    def test_multi_frame_export_selects_nested_svg_states(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "item.swf"
            source.write_bytes(b"FWS")
            request = character_svg.SymbolRequest("item", source, "Item", 7, 3)
            observed_command = []

            def fake_run(command, **_kwargs):
                observed_command.extend(command)
                export_index = command.index("-export")
                output = Path(command[export_index + 2])
                frame_dir = output / "DefineSprite_7_Item" / "3"
                frame_dir.mkdir(parents=True)
                for frame in range(1, 4):
                    (frame_dir / f"{frame}.svg").write_text("<svg/>", encoding="utf-8")
                return mock.Mock(returncode=0, stderr="", stdout="")

            with mock.patch.object(character_svg.subprocess, "run", side_effect=fake_run):
                exported = character_svg.export_requested_symbol_frames(
                    [request],
                    ffdec=root / "ffdec.jar",
                    zoom=1,
                    destination=root / "export",
                    subframe_start=2,
                    frame_count=2,
                )

            self.assertEqual([path.name for path in exported["item"]], ["2.svg", "3.svg"])
            sublength_index = observed_command.index("-sublength")
            self.assertEqual(observed_command[sublength_index + 1], "3")

    def test_legacy_unnamed_sprite_export_uses_generic_ffdec_directory(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "axe05.swf"
            source.write_bytes(b"FWS")
            request = character_svg.SymbolRequest("weapon", source, "axe05", 12, 1)

            def fake_run(command, **_kwargs):
                export_index = command.index("-export")
                output = Path(command[export_index + 2])
                directory = output / "DefineSprite_12"
                directory.mkdir(parents=True)
                (directory / "1.svg").write_text("<svg/>", encoding="utf-8")
                return mock.Mock(returncode=0, stderr="", stdout="")

            with mock.patch.object(character_svg.subprocess, "run", side_effect=fake_run):
                exported = character_svg.export_requested_symbol_frames(
                    [request],
                    ffdec=root / "ffdec.jar",
                    zoom=1,
                    destination=root / "out",
                )

            self.assertEqual(exported["weapon"][0].name, "1.svg")

    def test_reads_swf_header_frame_rate(self):
        with tempfile.TemporaryDirectory() as temporary:
            source = Path(temporary) / "renderer.swf"
            # Zero-bit RECT, 24 FPS in SWF's little-endian 8.8 field, one frame.
            body = b"\x08\x00" + b"\x00\x18" + b"\x01\x00" + b"\x00\x00"
            source.write_bytes(
                b"FWS" + b"\x09" + struct.pack("<I", 8 + len(body)) + body
            )

            self.assertEqual(character_svg.swf_frame_rate(source), 24)

    def test_24_fps_webp_durations_have_no_cumulative_rounding_drift(self):
        durations = character_svg.frame_durations_for_rate(6, 24)

        self.assertEqual(durations, [42, 41, 42, 42, 41, 42])
        self.assertEqual(sum(durations), 250)

    def test_animation_delta_crop_expands_offsets_to_even_coordinates(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            previous = root / "previous.png"
            current = root / "current.png"
            Image.new("RGBA", (8, 8), (0, 0, 0, 0)).save(previous)
            changed = Image.new("RGBA", (8, 8), (0, 0, 0, 0))
            changed.putpixel((3, 5), (255, 0, 0, 255))
            changed.save(current)

            self.assertEqual(
                character_svg.animation_delta_crop(current, previous),
                (2, 4, 2, 2, (8, 8)),
            )
            self.assertEqual(
                character_svg.animation_delta_crop(previous, previous),
                (0, 0, 1, 1, (8, 8)),
            )
            self.assertEqual(
                character_svg.animation_delta_crop(current, None),
                (0, 0, 8, 8, (8, 8)),
            )

    def test_expanded_frame_durations_validate_count(self):
        self.assertEqual(character_svg.expanded_frame_durations(3, 42), [42, 42, 42])
        self.assertEqual(
            character_svg.expanded_frame_durations(3, [42, 41, 42]),
            [42, 41, 42],
        )
        with self.assertRaisesRegex(character_svg.CharacterSvgError, "durations"):
            character_svg.expanded_frame_durations(3, [42, 41])

    def test_detects_shortest_complete_combined_nested_loop(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            exports: dict[str, list[Path]] = {"two": [], "three": []}
            for frame_index in range(10):
                for key, period in (("two", 2), ("three", 3)):
                    path = root / f"{key}-{frame_index}.svg"
                    path.write_text(str(frame_index % period), encoding="utf-8")
                    exports[key].append(path)

            self.assertEqual(
                character_svg.detect_complete_loop_frame_count(
                    exports,
                    max_frames=6,
                    validation_frames=4,
                ),
                6,
            )

    def test_complete_loop_detection_returns_none_at_too_small_cap(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            paths = []
            for frame_index in range(8):
                path = root / f"frame-{frame_index}.svg"
                path.write_text(str(frame_index % 6), encoding="utf-8")
                paths.append(path)

            self.assertIsNone(
                character_svg.detect_complete_loop_frame_count(
                    {"item": paths},
                    max_frames=4,
                    validation_frames=4,
                )
            )

    def test_combined_loop_can_exceed_per_timeline_scan_cap(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            exports: dict[str, list[Path]] = {"four": [], "six": []}
            for frame_index in range(14):
                for key, period in (("four", 4), ("six", 6)):
                    path = root / f"{key}-{frame_index}.svg"
                    path.write_text(str(frame_index % period), encoding="utf-8")
                    exports[key].append(path)

            self.assertEqual(
                character_svg.detect_complete_loop_frame_count(
                    exports,
                    max_frames=6,
                    validation_frames=4,
                ),
                12,
            )

    def test_loop_drivers_ignore_blink_but_keep_independent_head_animation(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            exports: dict[str, list[Path]] = {
                "armor_head": [],
                "helm": [],
                "hair": [],
                "cape": [],
            }
            periods = {"armor_head": 3, "helm": 3, "hair": 4, "cape": 2}
            for frame_index in range(10):
                for key, period in periods.items():
                    path = root / f"{key}-{frame_index}.svg"
                    path.write_text(
                        f"{key}:{frame_index % period}",
                        encoding="utf-8",
                    )
                    exports[key].append(path)

            drivers, ignored = character_svg.loop_driver_exports(exports)

            self.assertEqual(ignored, ("armor_head", "helm"))
            self.assertEqual(set(drivers), {"hair", "cape"})
            self.assertEqual(
                character_svg.detect_complete_loop_frame_count(
                    drivers,
                    max_frames=4,
                    validation_frames=4,
                ),
                4,
            )
            self.assertEqual(
                character_svg.detect_blink_frame_count(
                    exports,
                    max_frames=4,
                    validation_frames=4,
                ),
                3,
            )

    def test_one_blink_is_aligned_to_repeating_item_periods(self):
        self.assertEqual(character_svg.aligned_animation_frame_count(1, 87), 87)
        self.assertEqual(character_svg.aligned_animation_frame_count(10, 87), 90)
        self.assertEqual(character_svg.aligned_animation_frame_count(40, 87), 120)
        self.assertEqual(character_svg.one_shot_source_frame_index(0, one_shot_frames=87), 0)
        self.assertEqual(character_svg.one_shot_source_frame_index(86, one_shot_frames=87), 86)
        self.assertEqual(character_svg.one_shot_source_frame_index(87, one_shot_frames=87), 86)
        self.assertEqual(character_svg.one_shot_source_frame_index(119, one_shot_frames=87), 86)

    def test_complete_loop_and_fixed_frames_are_mutually_exclusive(self):
        parser = character_svg.build_parser()

        args = parser.parse_args(["Soltina", "--complete-loop", "--dry-run"])
        self.assertTrue(args.complete_loop)
        self.assertTrue(args.dry_run)
        self.assertIsNone(args.frames)
        self.assertIsNone(args.frame_duration)
        self.assertEqual(args.max_frames, character_svg.DEFAULT_LOOP_MAX_FRAMES)
        self.assertEqual(args.workers, min(4, character_svg.os.cpu_count() or 1))
        self.assertEqual(args.webp_encoder, "auto")
        self.assertEqual(args.webp_method, 4)
        self.assertIsNone(args.webp_lossy_quality)
        self.assertEqual(
            parser.parse_args(
                ["Soltina", "--webp-lossy-quality", "85"]
            ).webp_lossy_quality,
            85,
        )
        self.assertEqual(
            parser.parse_args(["Soltina", "--workers", "7"]).workers,
            7,
        )
        with mock.patch.object(parser, "_print_message"), self.assertRaises(SystemExit):
            parser.parse_args(["Soltina", "--frames", "8", "--complete-loop"])

    def test_numbered_output_paths_preserve_single_frame_name(self):
        base = Path("render/hero.svg")
        self.assertEqual(character_svg.numbered_output_paths(base, 1), [base])
        self.assertEqual(
            character_svg.numbered_output_paths(base, 3),
            [
                Path("render/hero-001.svg"),
                Path("render/hero-002.svg"),
                Path("render/hero-003.svg"),
            ],
        )

    def test_multi_frame_svgs_receive_one_shared_viewbox(self):
        with tempfile.TemporaryDirectory() as temporary:
            first = Path(temporary) / "first.svg"
            second = Path(temporary) / "second.svg"
            first.write_text(
                '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 10 10"/>',
                encoding="utf-8",
            )
            second.write_text(
                '<svg xmlns="http://www.w3.org/2000/svg" viewBox="5 -5 10 20"/>',
                encoding="utf-8",
            )

            shared = character_svg.align_frame_svg_viewboxes(
                [first, second],
                max_size=200,
                padding=0,
            )

            self.assertEqual(shared, (0.0, -5.0, 15.0, 20.0))
            self.assertEqual(item_renderer.svg_canvas_viewbox(first), shared)
            self.assertEqual(item_renderer.svg_canvas_viewbox(second), shared)
            root = ET.parse(first).getroot()
            self.assertEqual(root.get("width"), "150px")
            self.assertEqual(root.get("height"), "200px")

    def test_saved_flashvars_can_be_loaded_for_offline_rerun(self):
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary) / "profile" / "hero.json"
            character_svg.save_flashvars(
                {"strName": "Hero", "strGender": "F"},
                output,
            )

            self.assertEqual(
                character_svg.load_flashvars(output),
                {"strGender": "F", "strName": "Hero"},
            )

    def test_inverse_transform_round_trips_affine_matrix(self):
        matrix = character_svg.PART_TRANSFORMS["weapon"]
        inverse = character_svg.invert_transform(matrix)

        self.assertMatrixAlmostEqual(
            item_renderer.compose_transforms(matrix, inverse),
            character_svg.IDENTITY,
        )
        self.assertMatrixAlmostEqual(
            item_renderer.compose_transforms(inverse, matrix),
            character_svg.IDENTITY,
        )

    def test_detects_retained_game_space_svg_override(self):
        holder = character_svg.PART_TRANSFORMS["weapon"]
        saved_outer = (
            holder[0] / 3,
            holder[1] / 3,
            holder[2] / 3,
            holder[3] / 3,
            holder[4],
            holder[5],
        )
        with tempfile.TemporaryDirectory() as temporary:
            source = Path(temporary) / "weapon.svg"
            source.write_text(
                '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 20 10">'
                f'<g transform="{character_svg.matrix_text(saved_outer)}">'
                '<g transform="matrix(3 0 0 3 6 9)"><path d="M0 0h2v3z"/></g>'
                '</g></svg>',
                encoding="utf-8",
            )

            coordinate_space, zoom, source_transform = (
                character_svg.inspect_svg_override(
                    source,
                    slot="weapon",
                    weapon_type="Sword",
                )
            )

            self.assertEqual(coordinate_space, "character")
            self.assertEqual(zoom, 1.0)
            self.assertEqual(source_transform, holder)

    def test_character_space_import_removes_crop_but_retains_zoom(self):
        holder = character_svg.PART_TRANSFORMS["weapon"]
        saved_outer = (
            holder[0] / 3,
            holder[1] / 3,
            holder[2] / 3,
            holder[3] / 3,
            holder[4],
            holder[5],
        )
        with tempfile.TemporaryDirectory() as temporary:
            source = Path(temporary) / "weapon.svg"
            source.write_text(
                '<svg xmlns="http://www.w3.org/2000/svg" viewBox="1 2 20 10">'
                f'<g transform="{character_svg.matrix_text(saved_outer)}">'
                '<g transform="matrix(3 0 0 3 6 9)"><path d="M0 0h2v3z"/></g>'
                '</g></svg>',
                encoding="utf-8",
            )

            imported = character_svg.import_character_space_svg("override", source)

            outer = next(iter(imported.definition))
            frame = next(iter(outer))
            self.assertEqual(
                character_svg.parse_matrix(frame.get("transform")),
                (3.0, 0.0, 0.0, 3.0, 0.0, 0.0),
            )
            expected_shift_x = saved_outer[0] * 6 + saved_outer[2] * 9
            expected_shift_y = saved_outer[1] * 6 + saved_outer[3] * 9
            self.assertMatrixAlmostEqual(
                imported.bounds,
                (1 - expected_shift_x, 2 - expected_shift_y, 20, 10),
            )
            self.assertEqual(imported.export_zoom, 3)
            self.assertAlmostEqual(
                imported.minimum_stroke_scale,
                item_renderer.affine_geometric_scale(saved_outer),
            )

    def test_calibrates_ffdec_minimum_stroke_from_final_pixel_scale(self):
        root = ET.fromstring(
            '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 100 50" '
            'width="200px" height="100px">'
            '<path stroke-width="8" '
            'data-aqw-ffdec-compensated-stroke-width="8" '
            'data-aqw-authored-stroke-width="0.25" '
            'data-aqw-symbol-minimum-stroke-scale="0.5" '
            'data-aqw-layer-stroke-scale="2"/>'
            '<path stroke-width="8" '
            'data-aqw-ffdec-compensated-stroke-width="8" '
            'data-aqw-authored-stroke-width="6" '
            'data-aqw-symbol-minimum-stroke-scale="0.5" '
            'data-aqw-layer-stroke-scale="2"/>'
            '</svg>'
        )

        calibrated = character_svg.calibrate_minimum_strokes(root)

        widths = [float(path.get("stroke-width")) for path in root]
        self.assertEqual(calibrated, 2)
        self.assertEqual(widths, [4.0, 6.0])
        self.assertEqual(root.get("data-aqw-minimum-stroke-width"), "1px")

    def test_composition_clones_reused_symbols_for_each_layer_stroke_scale(self):
        marker = f"{{{character_svg.FFDEC_NS}}}has-small-stroke"
        original = f"{{{character_svg.FFDEC_NS}}}original-stroke-width"
        definition = ET.Element(
            f"{{{character_svg.SVG_NS}}}g",
            {"id": "symbol_piece"},
        )
        ET.SubElement(
            definition,
            f"{{{character_svg.SVG_NS}}}path",
            {
                "d": "M0 0L10 0",
                "fill": "none",
                "stroke": "#000",
                "stroke-width": "10",
                marker: "true",
                original: "0.1",
            },
        )
        symbol = character_svg.ImportedSymbol(
            "piece",
            definition,
            [],
            (0.0, 0.0, 10.0, 10.0),
        )
        layers = [
            character_svg.Layer("small", "piece", character_svg.IDENTITY),
            character_svg.Layer("large", "piece", (2, 0, 0, 2, 20, 0)),
        ]

        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary) / "character.svg"
            with mock.patch.object(character_svg, "CHARACTER_DISPLAY_SCALE", 1.0):
                character_svg.compose_svg(
                    {"piece": symbol},
                    layers,
                    fields={},
                    all_color_rules=[],
                    output=output,
                    max_size=100,
                    padding=0,
                    facing="right",
                    rsvg_convert=None,
                )
            root = ET.parse(output).getroot()

        paths = [
            element
            for element in root.iter()
            if element.tag.rsplit("}", 1)[-1] == "path"
        ]
        uses = [
            element
            for element in root.iter()
            if element.tag.rsplit("}", 1)[-1] == "use"
        ]
        self.assertEqual(sorted(float(path.get("stroke-width")) for path in paths), [2, 4])
        self.assertEqual(len({use.get("href") for use in uses}), 2)
        self.assertEqual(character_svg.unresolved_svg_references(root), set())

    def test_rebases_character_space_override_for_dagger_holders(self):
        layers = character_svg.build_layers(
            {"weapon": "svg_override"},
            weapon_type="Dagger",
        )
        rebased = character_svg.rebase_character_space_override(
            layers,
            symbol_key="svg_override",
            source_transform=character_svg.PART_TRANSFORMS["weapon"],
        )
        by_name = {layer.name: layer.transform for layer in rebased}

        self.assertMatrixAlmostEqual(by_name["weapon"], character_svg.IDENTITY)
        self.assertMatrixAlmostEqual(
            by_name["weapon_off"],
            item_renderer.compose_transforms(
                character_svg.PART_TRANSFORMS["weapon_off"],
                character_svg.invert_transform(
                    character_svg.PART_TRANSFORMS["weapon"]
                ),
            ),
        )

    def test_detects_raw_ffdec_svg_override(self):
        with tempfile.TemporaryDirectory() as temporary:
            source = Path(temporary) / "raw.svg"
            source.write_text(
                '<svg xmlns="http://www.w3.org/2000/svg" width="20px" height="10px">'
                '<g transform="matrix(2 0 0 2 6 8)"><path d="M0 0h2v3z"/></g>'
                '</svg>',
                encoding="utf-8",
            )

            coordinate_space, zoom, _ = character_svg.inspect_svg_override(
                source,
                slot="helm",
                weapon_type="Sword",
            )

            self.assertEqual(coordinate_space, "symbol")
            self.assertEqual(zoom, 2.0)

    def test_rejects_svg_without_recoverable_registration_point(self):
        with tempfile.TemporaryDirectory() as temporary:
            source = Path(temporary) / "generic.svg"
            source.write_text(
                '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 20 10">'
                '<path d="M0 0h2v3z"/></svg>',
                encoding="utf-8",
            )

            with self.assertRaisesRegex(
                character_svg.CharacterSvgError,
                "Cannot recover an AQW registration point",
            ):
                character_svg.inspect_svg_override(
                    source,
                    slot="weapon",
                    weapon_type="Sword",
                )

    def test_rejects_flat_armor_svg_override(self):
        with tempfile.TemporaryDirectory() as temporary:
            source = Path(temporary) / "armor.svg"
            source.write_text('<svg xmlns="http://www.w3.org/2000/svg"/>', encoding="utf-8")

            with self.assertRaisesRegex(
                character_svg.CharacterSvgError,
                "flat armor SVG cannot be overridden faithfully",
            ):
                character_svg.inspect_svg_override(
                    source,
                    slot="armor",
                    weapon_type="Sword",
                )

    def test_tint_shades_match_characterb_offsets(self):
        self.assertEqual(character_svg.tint_rgb(0x123456, "None"), (0x12, 0x34, 0x56))
        self.assertEqual(character_svg.tint_rgb(0x123456, "Light"), (118, 152, 186))
        self.assertEqual(character_svg.tint_rgb(0x123456, "Dark"), (0, 2, 36))
        self.assertEqual(character_svg.tint_rgb(0x123456, "Darker"), (0, 0, 0))

    def test_filters_use_exact_character_hair_eye_and_skin_colors(self):
        defs = ET.Element(f"{{{character_svg.SVG_NS}}}defs")
        warnings = character_svg.add_color_filters(
            defs,
            {("Hair", "None"), ("Eye", "None"), ("Skin", "None")},
            {
                "intColorHair": str(0x663300),
                "intColorEye": str(0x663300),
                "intColorSkin": str(0xE6BC93),
            },
        )
        self.assertEqual(warnings, [])
        matrices = {
            element.get("id"): next(iter(element)).get("values")
            for element in defs
        }
        self.assertEqual(
            matrices["aqw_tint_hair_none"],
            "0 0 0 0 0.4 0 0 0 0 0.2 0 0 0 0 0 0 0 0 1 0",
        )
        self.assertEqual(matrices["aqw_tint_eye_none"], matrices["aqw_tint_hair_none"])
        self.assertEqual(
            matrices["aqw_tint_skin_none"],
            "0 0 0 0 0.901960784 0 0 0 0 0.737254902 "
            "0 0 0 0 0.576470588 0 0 0 1 0",
        )

    def test_nested_flashvars_json_is_supported(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "character.json"
            path.write_text(
                json.dumps({"flashvars": {"strGender": "F", "strName": "Hero"}}),
                encoding="utf-8",
            )
            self.assertEqual(character_svg.load_flashvars(path)["strName"], "Hero")

    def test_unresolved_svg_references_are_reported(self):
        root = ET.fromstring(
            '<svg xmlns="http://www.w3.org/2000/svg">'
            '<defs><g id="present"/></defs><use href="#present"/>'
            '<use href="#missing"/></svg>'
        )
        self.assertEqual(character_svg.unresolved_svg_references(root), {"missing"})


if __name__ == "__main__":
    unittest.main()
