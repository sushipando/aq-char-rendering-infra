#!/usr/bin/env python3
"""Rebuild pinned static charpage artwork with FFDec; never deploys or calls AWS."""
from __future__ import annotations
import argparse
import copy
import gzip
import hashlib
import json
from pathlib import Path
import shutil
import subprocess
import urllib.request
import xml.etree.ElementTree as ET


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--ffdec", required=True, type=Path)
    parser.add_argument("--work-dir", required=True, type=Path)
    parser.add_argument("--output-dir", type=Path, default=Path(__file__).resolve().parents[1] / "services/pipeline-rust/assets/charpage")
    args = parser.parse_args()
    work = args.work_dir.resolve(); work.mkdir(parents=True, exist_ok=True)
    output = args.output_dir.resolve(); output.mkdir(parents=True, exist_ok=True)
    references = Path(__file__).resolve().parents[1] / "services/pipeline-rust/assets/charpage"
    java = ["java", "-Djava.awt.headless=true", f"-Duser.home={work / 'java-home'}", "-jar", str(args.ffdec.resolve())]

    def run(*parts):
        subprocess.run([*java, *map(str, parts)], check=True)

    def download(url, name, digest):
        path = work / name
        if not path.exists():
            with urllib.request.urlopen(url, timeout=30) as response:
                path.write_bytes(response.read())
        if hashlib.sha256(path.read_bytes()).hexdigest() != digest:
            raise ValueError(f"Pinned source checksum mismatch: {url}")
        return path

    def pack(source, name):
        (output / name).write_bytes(gzip.compress(source.read_bytes(), mtime=0))

    reference = json.loads((references / "character-source.json").read_text())
    swf = download(reference["url"], "characterB.swf", reference["sha256"])
    run("-swf2xml", swf, work / "character.xml")
    root = ET.parse(work / "character.xml").getroot()
    definitions, placements = [], []
    frame = 1
    for tag in root.find("tags"):
        kind = tag.get("type", "")
        if kind == "ShowFrameTag": frame += 1
        elif kind.startswith("Define") or kind == "JPEGTablesTag": definitions.append(tag)
        elif kind.startswith("PlaceObject") and frame == 12: placements.append(tag)
    # Only original artwork: no character, text fields, camera/cosmetics buttons,
    # scripts, event handlers, profile-picture UI, or placeholder text.
    depths = {"background": {1}, "fade": {3,5,7,9,73}, "chrome": {17,54,56,62,65,70}, "guild": {40}}
    for name, selected in depths.items():
        scene = copy.deepcopy(root); scene.set("frameCount", "1")
        tags = scene.find("tags"); tags.clear()
        for tag in definitions: tags.append(copy.deepcopy(tag))
        for tag in placements:
            if int(tag.get("depth")) in selected: tags.append(copy.deepcopy(tag))
        tags.append(ET.Element("item", type="ShowFrameTag")); tags.append(ET.Element("item", type="EndTag"))
        xml = work / f"{name}.xml"; movie = work / f"{name}.swf"
        ET.ElementTree(scene).write(xml, encoding="utf-8", xml_declaration=True)
        run("-xml2swf", xml, movie)
        run("-ignorebackground", "-format", "frame:svg", "-export", "frame", work / name, movie)
        pack(work / name / "1.svg", f"{name}.svgz")
    run("-selectid", "225", "-format", "sprite:svg", "-export", "sprite", work / "factions", swf)
    for frame, name in [(2,"neutral"),(3,"good"),(4,"evil"),(5,"chaos")]:
        pack(next((work / "factions").rglob(f"{frame}.svg")), f"faction-{name}.svgz")
    run("-export", "font", work / "fonts", swf)
    for source, target in [("138_BD Merced.ttf","name.ttf"),("195_Arial Black.ttf","body.ttf")]:
        shutil.copyfile(work / "fonts" / source, output / target)
    for record in json.loads((references / "sources.json").read_text()):
        index = record["index"]
        movie = download("https://game.aq.com/game/gamefiles/etc/chardetail/bgs/" + record["file"], f"background-{index}.swf", record["sha256"])
        destination = work / f"background-{index}"
        run("-select", "1", "-format", "frame:svg", "-export", "frame", destination, movie)
        pack(destination / "1.svg", f"background-{index}.svgz")


if __name__ == "__main__":
    main()
