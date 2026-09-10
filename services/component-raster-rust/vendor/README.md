# Vendored resvg (linebender/resvg)

Checked out at tag **v0.48.1** (`68b14c4 Prepare v0.48.1 release (#1111)`).

We removed the nested `.git` and vendor the tree as plain files so CDK Docker
builds work without submodule init. The original blur changes are described in
[`docs/resvg_blur_speed_optimization.md`](../../../docs/resvg_blur_speed_optimization.md).

The change is preserved as re-appliable .patch files (see `patch/`). To
create a fork/branch from upstream with the change applied:

```bash
# from this repo's root, the patches are at:
PATCHES=services/component-raster-rust/vendor/patch

git clone --depth 1 --branch v0.48.1 https://github.com/linebender/resvg
cd resvg
git switch -c aqw-blur-simd
git apply "$PATCHES/0001-add-libblur-simd-blur-feature.patch"
git apply "$PATCHES/0002-feGaussianBlur-libblur-backend.patch"
git add -A && git commit -m "Add libblur SIMD Gaussian blur backend (feGaussianBlur)"
# push to a fork / open a PR upstream:
#   git remote add fork git@github.com:sushipando/resvg.git
#   git push fork aqw-blur-simd
```

(Paths in the patches are `crates/...`, so apply from the resvg repo root.
The first two files preserve the original blur patch. Later changes also live
directly in the vendored tree.)

Files changed in our fork (relative to v0.48.1):
- `crates/resvg/Cargo.toml` — add optional `libblur` dep + `simd-blur` feature.
- `crates/resvg/src/filter/mod.rs` — `apply_blur` routes large-sigma feGaussianBlur
  (both axes >= 2, matching resvg's own box-blur threshold) through libblur's
  `gaussian_box_blur` (three-box / CLT SIMD approximation on premultiplied RGBA)
  when the `simd-blur` feature is on. The blur input is borrowed directly from
  tiny-skia (no Pixmap->Vec copy) and the destination transferred into a fresh
  Pixmap (no Vec->Pixmap copy), with one wrapper-owned allocation so an OOM is
  a graceful fallback to resvg's in-place blur. Runtime switches:
  `RESVG_BLUR_BACKEND=libblur|original` (default libblur),
  `RESVG_BLUR_THREADS=single|adaptive` (default single). Small sigma (< 2)
  and one-axis blurs keep upstream resvg IIR/box behavior.

The AQW Add blend extension and ordered layer rendering are described in
[`docs/render-fixes-river-and-additive-auras-2026-09-09.md`](../../../docs/render-fixes-river-and-additive-auras-2026-09-09.md).
They modify `usvg`'s blend parsing/tree and `resvg`'s group renderer, with
`resvg::layers` exposing ordered passes for final frame composition. The
`aqw-add` SVG value is a private extension, not portable CSS.
The changes are recorded in `patch/0003-additive-blend-and-layer-passes.patch`.
