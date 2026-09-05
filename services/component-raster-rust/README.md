# aqw-component-raster (Rust)

Native Rust replacement candidate for the Python component-raster Lambda
(`RasterComponentState`): it parses each unique placed component state from
its FFDec SVG export, assembles the tight-page component SVG (tint filters,
authored CXFORM filters, minimum-stroke calibration, font-zoom normalization,
reference rewriting), rasterizes **in-process with resvg as a library**
(usvg + tiny-skia, pinned to the same 0.48.1 as the Python image's CLI),
crops to visible alpha, downsamples onto the final output grid with
**fast_image_resize (FIR)** — separable Lanczos3 with SIMD, ~3x faster than
the Pillow-verbatim path with a measured <=9/255 premultiplied diff on real
content — and uploads the component PNG plus the full result record.

The Pillow-verbatim resampler (`resample.rs`, a bit-exact port of Pillow's
`Resample.c` + RGBa conversions) is retained and selectable with
`AQW_DOWNSAMPLER=exact`; it remains the parity reference and rollback path.
`scripts/rust_raster_parity.py` runs both pipelines over synthetic FFDec jobs
(zoom-2 with strokes/tints/CXFORMs/gradients/fonts, 2x and 1x downsampling)
and a real FFDec pet export: the FIR default passes a premultiplied-on-gray
tolerance gate (max 6/255, <0.01% significant pixels), and `--exact` requires
bit-identical RGBA.

## Optional local SVG comparison: ThorVG 1.1.1

Production and pipeline builds are resvg-only and do not compile or link the
vendored ThorVG C++ backend. To run a local comparison, opt into the Cargo
feature explicitly:

```bash
cargo run --release --features thorvg -- local-raster \
  --store-root /path/to/store --job-id JOB --task-index 0 \
  --raster-backend thorvg
```

- **Rust backend** — `src/thorvg.rs` drives the vendored ThorVG 1.1.1 C API
  (`tvg_engine_init` -> `tvg_swcanvas_create` -> `tvg_picture_load_data` ->
  `tvg_swcanvas_set_target(ARGB8888S)` -> add/draw/sync) into a caller-owned
  straight-alpha buffer at the exact page size, with the same alpha-bbox crop,
  output-grid downsample, and PNG encode as the resvg path.
- **Vendoring** — `vendor/thorvg-sys-upstream/` bundles the `thorvg-sys`
  0.3.2 crate (build.rs, hosted build via cc-rs) with the trimmed ThorVG 1.1.1
  C++ **source** (renderer + cpu_engine + svg/png/sfnt loaders + C API;
  lottie/gpu/webp/jpg/media are stripped). The Rust bindings are
  pre-generated into `bindings.rs` and the patched `build.rs` copies them
  instead of invoking bindgen, so the Lambda Docker build needs **no
  libclang**. `Cargo.toml` pins `thorvg-sys` with
  `features = [vendored, svg, png, fonts, threads]`.
- **Selection plumbing** — `local-raster --raster-backend thorvg` is accepted
  only by builds compiled with `--features thorvg`. The deployed pipeline
  contract accepts only `resvg`.
- **Cache** — `CACHE_SCHEMA` is bumped to `2` and the content-addressed key
  includes the backend, so resvg and thorvg entries never collide.
- **Notes** — ThorVG 1.1.1 starts C-API paints at refcount 0, so teardown
  must call `tvg_paint_rel` *before* `tvg_canvas_destroy` (rel is a no-op for
  canvas-adopted paints, a delete for unadopted ones — see `src/thorvg.rs`).
  The engine is process-global and its init/term refcount is not
  thread-safe, so the ThorVG unit test is a single serialized
  `engine_round_trip` test (production is one SVG per process anyway).
  Outputs are intentionally **not** pixel-identical to resvg; the parity
  harness compares ThorVG side channels geometrically (±1 px bbox) and
  reports the pixel delta as informational:

```bash
cargo build --release --features thorvg
uv run --package aqw-char-renderer python scripts/rust_raster_parity.py \
  --raster-backend thorvg
```

**CC support.** Character colors (tints) and authored placement CXFORMs are
applied through SVG `feColorMatrix type="matrix"` filters, which ThorVG
upstream does not implement (only `feGaussianBlur`). The vendored engine
adds a minimal feColorMatrix: a new SVG loader node type plus a
`SceneEffect::ColorMatrix` raster pass in `tvgSwPostEffect.cpp` that applies
row-major coefficients to straight sRGB channels per the spec. Only
`type="matrix"` is supported; other kinds degrade to the filter being
ignored (upstream's silent-skip behavior). Verified on real shrp data: the
cape's tinted cloth rendered the untinted base color before the patch and
the resvg-matching tint after, while CC-free parts stay pixel-identical to
resvg.

## Layout

```text
src/
  main.rs         runtime selection: Lambda runtime vs local-raster CLI
  lib.rs          library facade
  contract.rs     event / manifest-subset / result-record serde models
  svg.rs          mutable SVG DOM (parse via roxmltree, namespace-safe write)
  import.rs       import_ffdec_symbol: zoom removal, color rules, CXFORMs,
                  stroke markers, font-zoom normalization, id rewriting
  component_svg.rs tight-page component SVG assembly + tint/darken filters
  resample.rs     Pillow-exact Lanczos resize (Resample.c + RGBa conversions)
  raster.rs       usvg/resvg render, alpha bbox, crop, downsample, PNG encode
  compositor.rs   Pillow-exact source-over blend + RGBA canvas helpers
  storage.rs      S3 source/sink + filesystem store for parity fixtures
  worker.rs       run_raster_task orchestration + component_raster_profile
  local.rs        local-raster CLI over a FilesystemObjectStore mirror
  telemetry.rs    component_raster_profile / component_raster_complete logs
tests/
  raster_local.rs end-to-end local-raster integration tests (real binary)
```

FIR is the active backend. Exact-mode parity keeps `resample.rs` (a bit-exact
port of Pillow's `Resample.c`: float Lanczos kernel, 2^22 fixed-point
coefficients, 2^21 bias, two-pass, with the `RGBa` premultiply/unpremultiply
round trips) verified and available via `AQW_DOWNSAMPLER=exact`. Caveat: FIR
can move a component's alpha bbox by +-1 px at hard alpha edges (the parity
harness tolerates this for FIR and asserts exactness only under `--exact`).

## Local mode (parity fixtures)

The S3 layout is mirrored under `<root>/work/<key>` (the same layout as the
Python `FilesystemObjectStore`), so the parity harness seeds one store per
worker and compares results directly:

```bash
cargo run --release --manifest-path services/component-raster-rust/Cargo.toml -- \
  local-raster --store-root DIR --job-id JOB --task-index 0
```

## Checks

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
cargo build --release
```

## Parity harness

Requires the resvg 0.48.1 CLI (the Python reference renders with the exact
binary the shared image uses) and the Rust release binary:

```bash
uv run --package aqw-char-renderer python scripts/rust_raster_parity.py        # FIR default (tolerance gate)
uv run --package aqw-char-renderer python scripts/rust_raster_parity.py --exact  # bit-identical Pillow parity
```
