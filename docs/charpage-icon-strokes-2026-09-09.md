# Charpage icon stroke correction

The information panel now uses the character renderer's minimum-stroke
calibration for bundled FFDec artwork. Previously, an outline that FFDec had
inflated to one pixel at its original export size became roughly 3.72 pixels
wide in a 2048-wide charpage.

The correction covers equipment icons (five marked paths), Good (six), Chaos
(ten), and Neutral (two). Evil and the guild symbol use filled outlines and
have no such markers. Generated text retains its authored outline.

`charpage::draw` decompresses the bundled SVG, detects FFDec minimum-stroke
markers, and applies the shared calibration before usvg expands references and
converts strokes. The bundled exports retain their original zoom-1 transforms,
so FFDec's compensated widths already include the internal placement scales.
Calibration accounts for the additional scale from the exported canvas to the
actual output canvas, including rounded heights and arbitrary aspect ratios.

Each marked width becomes `max(authored_width, compensated_width / output_scale)`.
This keeps hairlines at a one-pixel minimum while allowing genuinely thicker
authored outlines to scale normally. The panel is drawn directly at output
resolution once per job; character components retain their existing raster-size
calibration and downsampling behavior. No extra frame rendering or encoding
pass is added.

The presentation policy is now `charpage-v4-icon-strokes`. It participates in
the final render cache key, so old presentation images are not reused. Original
SWFs, vector artwork, and character raster cache policies are unchanged.

## Validation

- Synthetic rendered strokes at widths 275, 550, 1100, 2048, and 4096 verify
  the one-pixel minimum, preservation of thicker authored widths, and unchanged
  ordinary strokes and filled geometry.
- Bundled equipment and all faction/guild icons are compared before/after at
  widths 550, 1100, and 2048. Native-size artwork is pixel-identical. Scaled
  marked outlines become thinner; Evil and guild artwork stay pixel-identical.
- Cropped 2048px comparisons were visually inspected for equipment, Good, and
  Chaos. These are local correctness checks, not AWS performance benchmarks.
- Pipeline unit/integration checks passed (149 tests; 20 fixture-dependent
  tests ignored), as did all raster unit/integration checks (48 tests).

Run the reusable icon checks, optionally saving comparison PNGs:

```sh
AQW_TEST_CHARPAGE_STROKE_OUTPUT=/tmp/aqw-charpage-strokes \
cargo test --manifest-path services/pipeline-rust/Cargo.toml \
  --lib charpage:: -- --nocapture
```

Implementation: [charpage drawing](../services/pipeline-rust/src/charpage.rs),
[shared calibration](../services/component-raster-rust/src/import.rs), and
[pixel regressions](../services/pipeline-rust/src/charpage/stroke_tests.rs).

This fix requires the updated pipeline image. Container builds and deployment
remain with the user. After deployment, rerender a charpage at size 2048; no
cache purge or original asset download is needed for this correction.
