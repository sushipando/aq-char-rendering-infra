# Vendored resvg (linebender/resvg)

Checked out at tag **v0.48.1** (`68b14c4 Prepare v0.48.1 release (#1111)`).

We removed the nested `.git` and vendor the tree as plain files so CDK Docker
builds work without submodule init. Our change is exactly one patch on top of
that tag; see
[`docs/resvg_blur_speed_optimization.md`](../../../docs/resvg_blur_speed_optimization.md)
for the design.

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

(Paths in the patches are `crates/resvg/...`, so apply from the resvg repo
root. Verified: applying both to a fresh v0.48.1 clone reproduces this
vendored tree byte-for-byte.)

Files changed in our fork (relative to v0.48.1):
- `crates/resvg/Cargo.toml` — add optional `libblur` dep + `simd-blur` feature.
- `crates/resvg/src/filter/mod.rs` — `apply_blur` routes feGaussianBlur through
  libblur (premultiplied RGBA) when the `simd-blur` feature is on, with
  runtime `RESVG_BLUR_BACKEND=libblur|original` (default libblur),
  `RESVG_BLUR_MODE=exact|fixed` (default exact), `RESVG_BLUR_EDGE=clamp|wrap|reflect`
  (default reflect).
