# Render presentation modes

Implemented 2026-09-07. Deployment and ARM builds remain owner-run.

## Discord

`/render-charpage` is test-guild-only. It uses the existing render admission,
per-user limits, result delivery, upload-size handling, and AVIF/WebP metadata.
`/render` keeps its existing controls and transparent, fitted character view.

Examples:

```text
/render-charpage username:___cj output_format:avif output_size:2048 max_frames:120
/render-charpage username:___cj info:false
/render-charpage username:___cj background:false info:false
/render-charpage username:___cj view:character background:true info:false facing:right
/render-charpage username:___cj character_x:400 character_y:304.2 canvas_width:650
```

The command exposes a master `use_cache` toggle; `output_format`, `lossless`,
`quality`, `webp_method`, and `avif_speed`; frame count and animation controls;
and `output_size` / `raster_size`. `quality` applies to the selected format.
`lossless:true` works for both formats. AVIF retains zstd-compressed lossless
RGBA intermediates. Default facing is left to match the supplied charpage
screenshot; `facing:right` flips the character without flipping text/background.

`background` and `info` independently override the selected preset.
`framing`, `canvas_width`, `canvas_height`, `character_x`, and `character_y`
control layout. Canvas dimensions/positions use logical layout coordinates,
independent of the output pixel resolution. The original stage is 550×350,
with the character registered at (338.05, 304.2). `output_size:2048` therefore
produces approximately 2048×1303 pixels, with exact height following the
existing two-stage raster/output rounding.

In content framing, the viewport follows the character's full animation bounds.
Moving the character also moves that fitted viewport, so registration changes
have no visible effect. Use fixed framing to position the character within a
stationary canvas. Fixed framing intentionally clips oversized equipment at
its edges, like the original charpage. It does not automatically shrink the
whole character to accommodate large weapons/pets.

## Shared layout contract

`render.view` selects a preset, while optional `render.presentation` overrides
individual layout properties. These controls are independent of output codecs,
character rasterization, and the Discord command name.

| Property | Character preset | Charpage preset |
|---|---|---|
| `framing` | `content` | `fixed` |
| `viewport` | unused while fitting | `[0,0,550,350]` |
| `character_position` | `[0,0]` | `[338.05,304.2]` |
| `background` | false | true |
| `info` | false | true |
| `border_fade` | true (when background enabled) | true |
| `border_color` | `#FEF0C1` | `#FEF0C1` |

Example request fragment for the existing fitted view with a background:

```json
{"render":{"view":"character","presentation":{"background":true,"info":false}}}
```

Fixed framing with a custom viewport and no text:

```json
{"render":{"view":"charpage","presentation":{"viewport":[0,0,650,350],"character_position":[400,304.2],"info":false}}}
```

These fragments also work with `/retry-render`'s `overrides` JSON. Retries inherit
saved presentation settings. If changing a saved, fully expanded preset, set
`"presentation":null` alongside the new `view` to reset its overrides.

CLI equivalents:

```sh
scripts/render-character ___cj --view charpage --format avif
scripts/render-character ___cj --view character --background --no-info
scripts/render-character ___cj --view charpage --no-info --viewport 0 0 650 350 --character-position 400 304.2
```

## Implementation and reuse

`presentation.rs` resolves/validates presets, calculates the viewport, and matches
the existing composer's canvas rounding. Character placement is implemented by
shifting the viewport relative to its registration point; component transforms,
color correction, raster backend, and animation selection stay shared.

`charpage.rs` supplies the original AQW background and information artwork.
Backgrounds cover the selected viewport without changing their aspect ratio;
the original fade follows the viewport edges. The information overlay fits
inside the viewport, anchored at its top-left. It uses the original embedded
fonts and equipment/guild/faction icons, with XML escaping and shrinking for long
text. Equipment labels reflect the same base/cosmetic and visibility selections
as the character metadata. Profile Pic and Cosmetics controls are excluded.

Selectable backgrounds now use their original SWF timelines. Resolve fetches the
pinned official SWF through the existing source cache, wraps its complete stage
as a nested sprite, and adds it to the ordinary export/bounds/raster pipeline.
Each distinct background state is rasterized by the existing distributed
component workers and reused through their cache. Background frames participate
in composition deduplication, so character repeats cannot freeze a moving scene.

Character bounds determine fitted framing; the background covers that viewport
independently and never mirrors with the character. The beige base, edge fade,
and information overlay remain static PNG layers, decoded once per compose
batch. `presentation_layers.background_overlay` is composited after the first
background component and before the character; `.foreground` follows all
components. No additional workflow node, GPU, or browser is needed. The pipeline
and compose images both require updating for this manifest extension.

The embedded bundle still supplies the default background, information artwork,
and static preview fallbacks for all 35 selections (base-36 `bgindex`). Runtime
selectable backgrounds use the pinned source files in `assets/charpage/sources.json`,
not these first-frame previews. Missing uncached SWFs require official asset
fallback to be enabled, as with character items. The builder continues to rebuild
preview artwork; it does not flatten runtime animation. Layout/artwork policies,
presentation settings, source checksums, and faction participate in cache keys.

Authored background timeline lengths participate in frame-count and loop metadata.
A quiet initial span is not evidence of a static background. `max_frames` and the
component frame cap still apply; long effects or complete cycles can require a
larger cap. Adding actual motion can increase encoded size and raster work, but
format, quality, lossless controls, and zstd frame transport remain unchanged.
Metadata includes the rendered background selection under General Info (for
example `Background W (32)`), or `None` when backgrounds are disabled.
Legacy Python rendering stages reject custom presentation rather than silently
producing the wrong view; production uses the Rust pipeline.

## Validation and rollout

Local tests cover independent layout controls, invalid coordinates/types,
odd-size canvas rounding, static-layer ordering for both WebP and zstd AVIF,
prepared manifest/artifact consistency, guild restrictions, queue settings,
and retries. A local one-frame render of ___cj uses the actual official SWFs,
the prepare/bounds/component raster path, and the shared compositor.

Local preview tools (no AWS):

```sh
cargo run --manifest-path services/pipeline-rust/Cargo.toml --example charpage_preview -- FIELDS.json /tmp/card-art 1024
cargo run --manifest-path services/pipeline-rust/Cargo.toml --example charpage_render_local -- FIELDS.json characterB.swf ffdec.jar /tmp/card-render --allow-official-downloads --facing-left
```

Update the pipeline and component-compose images together, then restart the bot
to register `/render-charpage`. No new infrastructure resources or IAM grants
are required. Local macOS results validate functionality and appearance, not
Graviton latency or encoded AWS animation sizes; those remain owner-run checks.

Local still-frame preview of `___cj` (1024-pixel output, original assets;
animated items are sampled at their first frame):

![Local charpage preview](images/charpage-preview.png)


## Border fade controls

`/render-charpage` accepts `border_color` (six hex digits, optionally prefixed
with `#`) and `border_fade` (boolean). The default remains a pale yellow fade.
Examples:

```text
/render-charpage username:fleki border_color:#112233
/render-charpage username:fleki border_fade:false
```

These map to `render.presentation.border_color` and `.border_fade`. Colors
normalize to uppercase `#RRGGBB`, so equivalent spellings share request/cache
identity. Invalid colors are rejected before Discord admission. The command
now has 25 options, within Discord's limit.

Both static and animated backgrounds use the same authored fade geometry and
opacity, recolored to the selected RGB. Disabling the fade also removes its
colored backing; uncovered artwork areas can be transparent. Border controls
have no visible effect when the background itself is disabled. Neither character
pixels nor information text are recolored. Retries preserve these settings.

CLI equivalents are `--border-color '#112233'` and `--no-border-fade`.
The presentation artwork policy is `charpage-v3-border-options`.
