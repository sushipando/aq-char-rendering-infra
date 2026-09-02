# aqw-component-raster (Rust)

Native Rust replacement candidate for the Python component-raster Lambda
(`RasterComponentState`): it parses each unique placed component state from
its FFDec SVG export, assembles the tight-page component SVG (tint filters,
authored CXFORM filters, minimum-stroke calibration, font-zoom normalization,
reference rewriting), rasterizes **in-process with resvg as a library**
(usvg + tiny-skia, pinned to the same 0.48.1 as the Python image's CLI),
crops to visible alpha, downsamples onto the final output grid with a
**Pillow-exact premultiplied-Lanczos resampler**, and uploads the component
PNG plus the full result record.

The output is pixel-for-pixel identical to the Python worker:
`scripts/rust_raster_parity.py` runs both pipelines over synthetic FFDec jobs
(zoom-2 with strokes/tints/CXFORMs/gradients/fonts, 2x and 1x downsampling)
and a real FFDec pet export, then requires exact RGBA equality.

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

`fast_image_resize` remains a dependency and can be selected with
`AQW_DOWNSAMPLER=fast_image_resize` for benchmarks; the default is the
Pillow-exact resampler because a single 1/255 alpha difference at an AA edge
can move the recorded bbox by a row.

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
uv run --package aqw-char-renderer python scripts/rust_raster_parity.py
```