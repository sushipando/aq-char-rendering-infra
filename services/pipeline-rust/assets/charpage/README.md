# Original AQW charpage artwork

Extracted from the official Artix Entertainment characterB SWF and its background
SWFs on 2026-09-07 using FFDec 26.2.1. These are original game assets, not newly
licensed project artwork. Source URLs and SHA-256 checksums are in
`character-source.json` and `sources.json`.

`background.svgz` is the default background. Backgrounds 1–35 match characterB's
background array; `bgindex` is base 36. Each background uses its first frame.
The card uses the original 550×350 stage, fade artwork, equipment/guild/faction
icons, and embedded BD Merced / Arial Black fonts. No profile/cosmetics controls
are included. SVGZ keeps these vector assets compact in the pipeline binary.

Rebuild (offline if the pinned SWFs already exist in the work directory):

```sh
python scripts/build_charpage_assets.py --ffdec /path/to/ffdec.jar --work-dir /tmp/charpage-assets
```

Any deliberate artwork/layout changes must increment `charpage::POLICY` so final
render cache entries cannot retain the previous presentation.
