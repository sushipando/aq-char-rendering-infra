# ThorVG Character Color (CC) Fix — feColorMatrix Patch + Findings

## Goal

The ThorVG 1.1.1 renderer was producing characters with **wrong colors**:
CC (character customization — tints, authored placement CXFORMs, and the
back-part "dark" filter) was silently dropped, so items rendered in their
base colors instead of the character's custom colors. This document records
the investigation, the fix (a minimal `feColorMatrix type="matrix"`
implementation in the vendored ThorVG engine), and the alternatives we
weighed — because "could we just compute these beforehand?" is a good
question and the answer shapes where correctness lives.

The details are for the `component-raster-rust` worker
(`services/component-raster-rust/`) and its vendored ThorVG at
`vendor/thorvg-sys-upstream/thorvg/`.

---

## Evidence

Reported on the character **shrp**, two diffs by `render.raster_backend`:

| job | backend | state |
|---|---|---|
| `bc9618a5-e72e-4972-b21a-0d1274cc41af` | thorvg | SUCCEEDED (colors wrong) |
| `1c622710-f73e-4b68-bdad-131ca4744939` | resvg | SUCCEEDED (colors right baseline) |

Comparing the delivered WebP mid-frames:

```text
shape (1864, 2048)  max channel diff 255  mean 6.97
pixels > 24/255:    668 536 (17.5%)
```

Comparing the cape component raster (deployed thorvg vs resvg reference), the
top opaque colors showed it clearly:

```
resvg-reference   #996666 (tinted brown)  1719 px
thorvg (deployed) #6699ff (base blue)     1636 px   <- CC dropped
```

After the fix, thorvg produced `#986565` ≈ resvg `#996666` and CC-free parts
became pixel-identical.

---

## Why CC works through `feColorMatrix`

The pipeline applies character customization as SVG **filters**, not as
repainted fills (port of the legacy "apply color filters" logic):

- **Tints** (`character_svg` / `component_svg.rs` `add_color_filters`) —
  for each `(location, shade)` rule, an offset-only matrix:

  ```text
  0 0 0 0 R   0 0 0 0 G   0 0 0 0 B   0 0 0 1 0   (offset-only tint)
  ```

- **Authored placement CXFORMs** (`import.rs` `authored_color_filter`) —
  Flash `ColorTransform` multiplier + offset, diagonal rows **including the
  alpha row**:

  ```text
  Rm 0  0  0  Roff    0  Gm 0  0  Goff    0  0  Bm 0  Boff    0  0  0  Am Aoff
  ```

- **Back-part darken** — constant black with alpha passthrough:
  `0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 1 0`.

These are grouped `<filter>` elements referenced by each painted layer. Both
engines receive the identical component SVG (shared `build_component_svg` /
`import.rs`), which is why the divergence is purely a renderer capability
difference.

---

## Root cause: ThorVG 1.1.1 implements only `feGaussianBlur`

ThorVG's SVG loader enumerates exactly one filter primitive:

```cpp
// tvgSvgLoader.cpp
"feGaussianBlur", sizeof("feGaussianBlur"), _createGaussianBlurNode
```

Grep for `feColorMatrix` / `feComposite` / `feFlood` in
`thorvg/src/loaders/svg/` — zero matches. Its builder `_applyFilter`
iterates the filter's children and only acts on `GaussianBlur`; every other
primitive is **silently skipped** and the paint is rendered unmodified.

So: resvg tinted everything and looked right; ThorVG rendered the item's
**base** colors (CC dropped), retained position/AA (bbox nearly identical,
same crop math) — hence "colors wrong but geometry ok", 17.5% of pixels
diffing.

---

## Fix: minimal `feColorMatrix type="matrix"` in the vendored engine

Committed as `Implement SVG feColorMatrix in vendored ThorVG (fixes CC
under ThorVG)`. Files changed inside
`vendor/thorvg-sys-upstream/thorvg/`:

```text
inc/thorvg.h                              SceneEffect::ColorMatrix enum entry
src/loaders/svg/tvgSvgCommon.h            SvgNodeType::ColorMatrix + SvgColorMatrixNode
src/loaders/svg/tvgSvgLoader.cpp          feColorMatrix tag factory + attr parse
src/loaders/svg/tvgSvgBuilder.cpp         _applyFilter: < -> SceneEffect::ColorMatrix
src/renderer/tvgRender.h                  RenderEffectColorMatrix struct
src/renderer/tvgScene.h                   Scene::add / duplicate switch cases
src/renderer/cpu_engine/tvgSwCommon.h     effectColorMatrix{Update,} decls
src/renderer/cpu_engine/tvgSwRenderer.cpp prepare()/render() dispatch
src/renderer/cpu_engine/tvgSwPostEffect.cpp  the per-pixel raster pass
```

### Loader

- New node type + struct holding `float values[20]` and a `valid` flag.
- `_attrParseColorMatrixNode` only accepts `type="matrix"` + parses the 20
  coefficients (`_parseNumber`). Anything else marks `valid = false`, and
  `_applyFilter` skips it — matching upstream's existing "unknown primitive =
  ignore" behavior (graceful degradation, no hard error).

### Builder

`_applyFilter` iterates children; for each `ColorMatrix` child it calls
`scene->add(SceneEffect::ColorMatrix, 20 doubles...)` with the row-major
coefficients. Ordering: multiple primitives stack in document order (the
pipeline today uses exactly one matrix per filter, so stacking wasn't a
constraint, but it follows the existing loop structure).

### Renderer / raster

New `SceneEffect::ColorMatrix` (value after `Tritone`), `RenderEffectColorMatrix`
with `float matrix[20]`, `SwRenderer::prepare`/`render` dispatch, and the pass
in `tvgSwPostEffect.cpp`:

```
for each pixel in effect bbox:
    src     = rasterUnpremultiply(pixel)      # engine buffers are premult ARGB
    r,g,b,a = channels in [0,1]               # SVG coefficients are normalized
    for row 0..3:
        out = Σ (matrix[row*5 + k] * ch[k]) + matrix[row*5 + 4]
        out = clamp(out, 0, 1)
    result = join(out channels)               # straight
    write  = premultiply(result) for the compositing pipeline
```

Direct-indirect behavior mirrors ThorVG's existing `effectTint` /
`effectTritone`: in the direct path it PLACES the color over what's beneath
using the **transformed alpha** as coverage (so an alpha-multiplying row
composites correctly); in the indirect path it replaces the compositor
buffer. The `#`256/255 alpha lerp yields off-by-one rounding at hard edges —
tests assert ±1 / ±3 tolerances.

Why straight-alpha math: the filter coefficients are specified on straight
(un-premultiplied) RGB in SVG, and our ThorVG surface is retasked to
`ARGB8888S` (straight) already; the engine, however, composites premultiplied
internally, so `rasterUnpremultiply` → matrix → `PREMULTIPLY` is required.

### The two "red herring" bugs during development

1. **Ambiguity between "malformed SVG" and "feature missing"**: the
   `thorvg::render_svg` wrapper rejects a rasterized size mismatch (expected
   6x4 vs "rendered 0x0") — escaped quotes in a test fixture (`\"` inside a
   raw string) made the SVG malformed; the size became 0×0 and the wrapper
   dressed it up as a backend failure. Raw strings in tests must not contain
   `\"` escapes.
2. **Coefficient normalization**: the first pass converted channels to
   0..255 and applied SVG offsets literally, plainly sockets "1" as "255" —
   the output was black/under-tinted. The SVG spec defines all coefficients
   in [0, 1], so channels are normalized too. After fixing, the probe
   showed `#feff7f00` ≈ (255, 128, 0) for a 0.5G tint.

Also: the ThorVG engine teardown requires `tvg_paint_rel(picture)` **before**
`tvg_canvas_destroy` (refcount 0 at creation; destroy frees adopted paints) —
already documented in `src/thorvg.rs`; we rediscovered a double-free risk
when debugging the segfault, but it was a stale probe binary (needed
`cargo clean` + rebuild) causing the "everything segfaults at exit" and not
this patch.

---

## Verification

- Unit tests (`src/thorvg.rs` `engine_round_trip`, extended): flat-tint
  matrix (0.5 green) → channels within ±1 of target and alpha preserved;
  diagonal CXFORM matrix (2x red + identity alpha on 50% opaque) → clamped
  red, ±3 on green/blue, alpha survives in (60,195).
- Real shrp `local-raster` (same manifest/bundles):
  - cape: top color `#986565` vs resvg `#996666` (AA + 1px crop); `>`-level
    diffs, mean 2.68/255, >64 only 0.01%.
  - armor_shoulder (CC-relevant): **pixel-identical** (max diff 0).
  - speed unchanged: cape rasterize resvg 385 ms → thorvg 39 ms.
- `cargo fmt -- --check`, `cargo clippy -D warnings`, `cargo test` all green.
- Must **redeploy** (`scripts/deploy_renderer.sh --yes`) for the live Lambda
  to pick up the new engine (the deployed thorvg raster on job
  bc9618a5… is the pre-fix binary, hence the blue cape).

---

## "Could we compute the colors beforehand instead of in ThorVG?"

Yes — with important stage assumptions. There are three stages, and the
"right" answer differs per matrix type.

### Stage A — SVG authoring time (repaint fills)

Feasible for the **flat tint rows** (`0 0 0 0 R ···`, character colors):
find the placed part's `fill` colors and repaint them to the character's
tint, dropping the filter.

Does **not** generalize:

- Authored placement CXFORMs (P-G multipliers including the alpha row) need
  the per-pixel incoming value; gradients, glows, textures, and AA edges
  only have those at draw time.
- The legacy Python renderer literally did this for try-on and hit the
  multiply-blend "isolate and flatten" problem — which is why the pipeline
  moved CC into a filter in the first place.

So: repaint is fine only for solid-color-only, non-multiplied artwork, where
you are prepared to break on the next cosmetic that uses gradients.
Everything else needs stage-C.

### Stage B — "compute the matrix values beforehand"

Only an option for the **values themselves** (e.g. you could cache/flat-tint
the decomposed rows, or hoist the authored CXFORM math up the chain), but
the per-pixel application still has to happen at raster time —
it's inherent to affine-per-pixel geometry. Precomputing the *values* is
safe and already done (the `feColorMatrix values=` are derived in Rust /
Python from `tint_rgb`, the authored transforms at Prepare/import.

### Stage C — raster time (post-pass in Rust, engine never sees filters)

This is the clean "compute beforehand to renderer" alternative:

- Render the same component SVG in ThorVG **with all filters placed**
  (ThorVG skips them, output = untinted), then, before the bbox crop +
  downsample, run a Rust pass over the returned RGBA applying the matrix
  (identity × row == . radius doesn't matter — the rows and offsets
  rows are affine per pixel, straight alpha, pixel-local, SIMDable).
- Since `build_component_svg` wraps each colored layer in *one* filter, the
  pass can tint the whole page (the filter wraps each of its painted layers); it runs on the final
  straight pixels so it captures gradients, textures, and multiply-blended
  layers exactly like the filter did.
- Trade-offs vs. the engine patch:
  - pros: no C++ code, no vendored dependency patching, unit-testable in
    pure Rust without rebuilding ThorVG, deterministic.
  - cons: the "truth" lives in two places (the SVG still AUTHORS a filter;
    the Rust pass re-implements what it means); any *ordering* with blending
    or blur primitives (not present today) would need the post-pass to
    interleave rather than a whole-page tint; you'd be trusting your
    an implicit invariant that the SVG's
    filters are exactly the CC rows.

### Verdict

- If the goal is **fidelity to the SVG** (any future filter primitive, always
  correct): the engine patch (done) is the honest answer.
- If the goal is **simplest correct code in Rust** with zero C++: the Stage C
  post-pass is legitimate for this pipeline (only offset/diagonal matrices,
  never combined with a blur today) and is a reasonable follow-up if speed of
  development / no-renderer-diffs become higher priority than "SVG is the
  sole source of truth".
- Stage A (repaint) is the only one you can actually "compute beforehand" in
  the authoring stage, and it's the least faithful to the artwork — treat it
  as a specialized optimization for known-flat tinted test items, not the
  general path.

---

## What else changed / how to confirm

- `scripts/thorvg.rs`'s engine_round_trip grew the two matrix cases so the
  behavior is locked by unit test.
- Inside `vendor/thorvg-sys-upstream`, this README-record covers how to
  re-apply if the vendor tree is bumped (the loader node, `SceneEffect`
  enum, `SwRenderer::prepare/render` switches, and `tvgSwPostEffect`
  functions are the four touch-points; `bindings.rs` freezes the C API and
  needs no change since the C API doesn't expose these internals).
- The deployed workflow runs the OLD image until the next `cdk deploy`;
  queueing a fresh render (`--raster-backend thorvg`) and verifying
  the brand-new WebP renders shrine's cape brown — not blue — is the
  end-to-end accept signal.