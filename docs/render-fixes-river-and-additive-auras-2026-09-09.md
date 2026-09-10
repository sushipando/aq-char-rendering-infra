# River export failure and missing additive auras

## Jobs and causes

| Job | Finding |
| --- | --- |
| `c8b9f03a-242f-404b-b1a6-8aee1dbbb29f` | `ExportSourceFrames` rejected AVM1 placement clip actions in `cp-river.swf`. Fetching succeeded. Two construction handlers only assign navigation metadata to invisible markers. |
| `e76eacdd-e55a-49e2-a65c-dfb90103d2e8` | The render succeeded but lost Flash Add blending and contained invalid blur parameters. Effects also need the underlying armor as their backdrop, which was lost by flattening each component separately. |

The River SWF has 65 sprites, each with one frame. It is static. Its marker
handlers assign `tCell`/`tPad`; sprite 107 also has an empty frame action. These
are not animation commands.

Taizou uses `HMDK25Gauntlet.swf` and `NecrophageOathbreakerArmor.swf`. The original
gauntlet has vertical blurs with horizontal blur zero. FFDec 26.2.1 converts this
to `stdDeviation="NaN ..."`. The gauntlet has one Add placement; the armor has
17. The unmodified SVG exporter drops that blend mode. Reproducing directly
from the cached original SWFs confirmed these export defects independently of
our timeline normalization.

## Changes

- AVM1 construction metadata is accepted generically for Load, Initialize and
  Construct events when the entire action block only assigns scalar literals to
  ordinary custom properties. Native display properties, callback names,
  target paths, playback commands in these handlers, dynamic reads/calls and
  unsupported events still fail explicitly. The supported playback evaluator
  cannot consume the omitted custom properties. This is not a general AVM1
  lifecycle interpreter.
- Validated metadata tails are removed from the temporary export copy. Original
  SWFs remain unchanged in S3. Matrix, color, filter, blend, mask and visibility
  fields are preserved byte for byte. Warnings record the accepted fields and
  placement context. Empty actions/metadata use the ordinary normalization path.
- A small, reproducible patch to FFDec exports Add as `mix-blend-mode: aqw-add`
  and clamps negative blur variances to zero before the square root. Other blur
  values retain the existing conversion. The build downloads two SHA-256-pinned
  source files, compiles only those classes, preserves the distribution's
  licenses and removes signatures invalidated by modifying the build copy.
- Vendored usvg/resvg handles the private Add extension on the CPU. It adds
  premultiplied RGB, uses source-over alpha, and clamps the result to valid
  premultiplied RGBA. Color-filter wrappers carry the blend operation on the
  outer wrapper so it sees the correct backdrop after color customization.
- Components with outward Add effects retain ordered normal/additive passes.
  Non-isolating groups are traversed; masks, filters and group opacity retain
  their isolation boundaries. Consecutive normal drawing shares a pass. Each
  pass is rasterized, cropped and encoded in turn, then reused across the
  component's frame appearances. The existing collector validates the layer
  keys, checksums, order and blend modes. The existing composer applies them
  against earlier equipment and the background.
- No new Step Functions states are needed. Ordinary components keep their
  existing path. Components containing Add currently bypass the shared single-PNG
  component cache; vector export caching and reuse within a job still work.
  There is a 128-pass limit per component.
  Experimental ThorVG requests use resvg for components with this extension.

Relevant implementations: [AVM1](../services/pipeline-rust/src/avm1.rs),
[FFDec patch builder](../services/pipeline-rust/ffdec/patch_svg_effects.py),
[layer planning](../services/component-raster-rust/vendor/resvg-upstream/crates/resvg/src/layers.rs),
[raster worker](../services/component-raster-rust/src/worker.rs),
[collector](../services/pipeline-rust/src/components.rs), and
[composer](../services/component-compose-rust/src/worker.rs).

The export policy is `rust-effective-svg-v14-construction-metadata-svg-effects`;
the bounds policy and component raster schema are also bumped. Old derived
images are invalidated; original SWFs remain cached. Final AVIF/WebP quality,
compression and metadata settings are unchanged. Restored visible effects can
change encoded file size. This investigation does not establish AWS latency or
file-size numbers; local Mac tests are correctness checks.

Upstream references: FFDec's
[SVG blend switch](https://github.com/jindrapetrik/jpexs-decompiler/blob/version26.2.1/libsrc/ffdec_lib/src/com/jpexs/decompiler/flash/exporters/commonshape/SVGExporter.java)
and [blur conversion](https://github.com/jindrapetrik/jpexs-decompiler/blob/version26.2.1/libsrc/ffdec_lib/src/com/jpexs/decompiler/flash/types/filters/SvgFiltering.java),
and Ruffle's [separate RGB/alpha Add equations](https://github.com/ruffle-rs/ruffle/blob/master/render/wgpu/src/blend.rs).

## Validation and deployment

Local checks covered generated AVM1 records and malformed/truncated variants;
placement effect preservation; additive pixels/alpha; color-wrapper scope;
layer planning versus a single SVG drawn against a backdrop; and the real
composer's PNG handoff, order, colors and rejection of an unknown blend mode.

Real SWFs passed export and warm-cache checks:

- River: 120 scheduled frames, one static SVG/raster state, two metadata warnings.
- Shadowfall: 120 scheduled frames, five distinct raster states; authored stops
  on sprites 145 and 235 remain intact.
- Taizou: 12 scheduled frames each for the gauntlet, chest and shoulder, with
  one, four and eight distinct raster states respectively. Blurs are finite,
  Add operations survive export/import, and effects stay animated.
- A saved-job first-frame preview used the actual character transforms and
  raster workers, then the collector and composition kernels. It restores the
  bright translucent red skeleton over the armor. This preview replaces the
  gauntlet/chest/shoulder exports; other components use the saved job's SVGs.
  It is a visual regression, not a pixel-identical recreation of the reference
  WebP's different facing, framing and animation phase.

Reusable tests:

```sh
cargo test --manifest-path services/pipeline-rust/Cargo.toml --lib --test pipeline --test svg_effects
cargo test --manifest-path services/component-raster-rust/Cargo.toml --lib --tests
cargo test --manifest-path services/component-compose-rust/Cargo.toml --lib --tests
```

For real-asset tests, patch a **copy** of the local FFDec distribution first:

```sh
cp -R /path/to/ffdec /tmp/aqw-effects-ffdec
python3 services/pipeline-rust/ffdec/patch_svg_effects.py /tmp/aqw-effects-ffdec

AQW_TEST_RIVER_WRAPPED_SWF=/path/to/river-wrapped.swf \
AQW_TEST_SHADOWFALL_WRAPPED_SWF=/path/to/shadowfall-wrapped.swf \
AQW_TEST_FFDEC=/tmp/aqw-effects-ffdec/ffdec.jar \
cargo test --manifest-path services/pipeline-rust/Cargo.toml \
  --test avm1_background -- --ignored --nocapture

# Directory contains this job's input.json and cached gauntlet.swf/armor.swf.
AQW_TEST_AURA_DIR=/path/to/taizou-fixtures \
AQW_TEST_FFDEC=/tmp/aqw-effects-ffdec/ffdec.jar \
cargo test --manifest-path services/pipeline-rust/Cargo.toml \
  --test svg_effects real_taizou -- --ignored --nocapture
```

The user should build/deploy the exporter, raster worker, pipeline functions
and composer together. The Dockerfile applies the FFDec patch during the build;
manual Lambda patching is unnecessary. No ARM container builds or deployments
were performed during the investigation. After deployment:

```text
/retry-render job:c8b9f03a-242f-404b-b1a6-8aee1dbbb29f render_cache:false
/retry-render job:e76eacdd-e55a-49e2-a65c-dfb90103d2e8 render_cache:false
```

## Separate finding: charpage icon outlines

**Follow-up:** this gap is now fixed; see
[charpage icon stroke correction](charpage-icon-strokes-2026-09-09.md).
The original investigation below describes the behavior before that correction.

The left information-panel icons bypassed character stroke calibration.
[charpage.rs](../services/pipeline-rust/src/charpage.rs) renders the bundled
`chrome.svgz` and faction SVGs directly. Character assets use
`prepare_minimum_strokes` and `calibrate_minimum_strokes` in the component import
and build path.

The equipment chrome has five FFDec minimum-stroke markers; Good has six and
Chaos has ten. Those compensated widths enlarged with output scale.
The numeric widths inside SVG definitions must be interpreted with their
transforms, not as final screen pixels. Evil has no such markers or stroked
paths: its outlines are filled geometry. The River/additive-aura commit did not
change these icons; the follow-up corrects them and also covers Neutral.
