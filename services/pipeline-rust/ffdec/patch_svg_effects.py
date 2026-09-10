#!/usr/bin/env python3
"""Patch FFDec 26.2.1 SVG effects in a build/local copy (never source SWFs).

Requires a JDK (javac and jar). Downloads two hash-pinned LGPL source files
from upstream GitHub unless --source-dir supplies their original contents.
Usage: python3 patch_svg_effects.py /path/to/ffdec [--source-dir DIR]
"""
import argparse
import hashlib
from pathlib import Path
import subprocess
import tempfile
import zipfile
from urllib.request import urlopen

VERSION = "aqw-svg-effects-v1"
BASE = "https://raw.githubusercontent.com/jindrapetrik/jpexs-decompiler/version26.2.1/libsrc/ffdec_lib/src/com/jpexs/decompiler/flash/"
SOURCES = {
    "exporters/commonshape/SVGExporter.java": "0ddce737e7210b8eabb1308ec65eedfd95852282983c8cc307d03757bdddd74c",
    "types/filters/SvgFiltering.java": "4f28f9e7730fead4c78d01b3ae1d165526ab08be7db7e3a115a91f2c07ee1af0",
}


def replace_once(text, old, new):
    if text.count(old) != 1:
        raise ValueError(f"FFDec patch context mismatch: {old}")
    return text.replace(old, new)


def patch(name, text):
    if name.endswith("SVGExporter.java"):
        return replace_once(text, "            case BlendMode.ADD:\n", """            case BlendMode.ADD:
                // AQW extension: RGB addition with source-over alpha, handled
                // by our CPU renderer. Do not substitute screen blending.
                element.setAttribute("style", "mix-blend-mode: aqw-add");
                break;
""")
    for axis in ("X", "Y"):
        text = replace_once(text,
            f"Math.sqrt((blur{axis}Scaled * blur{axis}Scaled - 1) / 12)",
            f"Math.sqrt(Math.max(0.0, (blur{axis}Scaled * blur{axis}Scaled - 1) / 12))")
    return text


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("ffdec", type=Path)
    parser.add_argument("--source-dir", type=Path)
    args = parser.parse_args()
    ffdec = args.ffdec.resolve()
    library = ffdec / "lib/ffdec_lib.jar"
    if not library.is_file():
        parser.error("expected FFDec distribution with lib/ffdec_lib.jar")
    with tempfile.TemporaryDirectory(prefix="aqw-ffdec-patch-") as directory:
        root = Path(directory)
        files = []
        for name, digest in SOURCES.items():
            data = (args.source_dir / Path(name).name).read_bytes() if args.source_dir else urlopen(BASE + name, timeout=60).read()
            if hashlib.sha256(data).hexdigest() != digest:
                raise ValueError(f"unexpected upstream source: {name}")
            target = root / "src/com/jpexs/decompiler/flash" / name
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_text(patch(name, data.decode()), encoding="utf-8")
            files.append(str(target))
        classes = root / "classes"
        classes.mkdir()
        subprocess.run(["javac", "--release", "17", "-encoding", "UTF-8", "-cp", str(ffdec / "lib/*"), "-d", str(classes), *files], check=True)
        (classes / VERSION).write_text(VERSION + "\n")
        subprocess.run(["jar", "uf", str(library), "-C", str(classes), "."], check=True)
        # This is a modified distribution, so the upstream JAR signature no
        # longer describes it. Keep licenses/resources; remove stale signatures
        # only in this explicitly selected build copy.
        unsigned = root / "ffdec_lib.jar"
        with zipfile.ZipFile(library) as old, zipfile.ZipFile(unsigned, "w") as new:
            for entry in old.infolist():
                name = entry.filename.upper()
                if name.startswith("META-INF/") and name.endswith((".SF", ".RSA", ".DSA", ".EC")):
                    continue
                new.writestr(entry, old.read(entry))
        library.write_bytes(unsigned.read_bytes())
    print(f"Patched {library}: {VERSION}")


if __name__ == "__main__":
    main()
