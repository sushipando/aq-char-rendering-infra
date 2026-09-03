# Vendored resvg (linebender/resvg)

Checked out at tag **v0.48.1** (`68b14c4 Prepare v0.48.1 release (#1111)`).

We removed the nested `.git` and vendor the tree as plain files so CDK Docker
builds work without submodule init. Our change is exactly one patch on top of
that tag; see
[`docs/resvg_blur_speed_optimization.md`](../../../docs/resvg_blur_speed_optimization.md)
for the design.

To re-apply the patch if you re-vendor from upstream:

```bash
git clone --depth 1 --branch v0.48.1 https://github.com/linebender/resvg resvg-upstream
cd resvg-upstream
git checkout -b aqw-blur-simd
# apply crates/resvg/Cargo.toml + crates/resvg/src/filter/mod.rs changes
```

Files changed in our fork (relative to v0.48.1):
- `crates/resvg/Cargo.toml` — add optional `libblur` dep + `simd-blur` feature.
- `crates/resvg/src/filter/mod.rs` — `apply_blur` routes feGaussianBlur through
  libblur (premultiplied RGBA) when the `simd-blur` feature is on, with
  runtime `RESVG_BLUR_BACKEND=libblur|original` (default libblur),
  `RESVG_BLUR_MODE=exact|fixed` (default exact), `RESVG_BLUR_EDGE=clamp|wrap|reflect`
  (default reflect).
