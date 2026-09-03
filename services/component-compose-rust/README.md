# aqw-component-compose (Rust)

Native Rust replacement candidate for the Python/Pillow
`ComposeComponentFrameChunk` Lambda (see
[`docs/rust-component-compose-plan.md`](../../docs/rust-component-compose-plan.md)).

The worker keeps the whole Step Functions contract: it accepts the existing
`ComposeComponentFrameChunk` event, decodes each referenced component PNG
once, premultiplies each layer once, draws layers in manifest order with an
AArch64 NEON SIMD Porter-Duff source-over kernel (via the `wide` crate),
unpremultiplies each frame once, encodes with the pinned `cwebp`, uploads
the WebP, and writes one compose-batch manifest. Missing components fail
the whole chunk.

## Compositor

The compositor works in **premultiplied RGBA8** (see
[`docs/arm64_neon_rgba_compositor_design.md`](../../docs/arm64_neon_rgba_compositor_design.md)):

```text
decoded straight RGBA8 layer
        ↓  premultiply once per layer (SIMD)
premultiplied RGBA8 layer
        ↓  source-over per layer (SIMD)
premultiplied RGBA8 frame
        ↓  unpremultiply once per frame (SIMD)
straight RGBA8 frame  →  PNG / WebP
```

`compositor/scalar.rs` is the permanent scalar reference; `compositor/wide.rs`
holds the SIMD kernels and must match it byte-for-byte (enforced by
`compositor/tests.rs`). `compositor.rs` keeps the legacy Pillow-exact
`blend_pixel` kernel for reference only.

## Layout

```text
src/
  main.rs        runtime selection: Lambda runtime vs local-compose CLI
  lib.rs         library facade
  contract.rs    event / manifest / batch-manifest serde models
  worker.rs      the shared chunk composer (one process, one decode each)
  compositor.rs  premultiplied-RGBA compositor facade + clipping
  compositor/scalar.rs  scalar reference (correctness oracle)
  compositor/wide.rs    AArch64 NEON SIMD kernels (`wide` crate)
  compositor/tests.rs   SIMD-vs-scalar byte-identity tests
  png.rs         PNG decode/encode (normalizes palette/gray/16-bit to RGBA8)
  encode.rs      pinned cwebp subprocess
  storage.rs     S3 source/sink
  local.rs       filesystem source/sink + local-compose CLI
  telemetry.rs   component_compose_profile / component_compose_complete logs
tests/
  local_mode.rs  end-to-end local-compose integration tests (real binary)
```

## Local mode

Reads the downloaded production artifact layout and runs one whole chunk
without mocking S3, retaining lossless frame PNGs for pixel comparison:

```bash
cargo run --release --manifest-path services/component-compose-rust/Cargo.toml -- \
  local-compose \
  --artifact-dir /private/tmp/alina-component-benchmark \
  --output-dir /private/tmp/rust-compose-run \
  --frame-start 1 \
  --frame-end 10
```

Writes `frames/{frame:06}.png` (lossless), `frames/{frame:06}.webp`, and
`batch-{index:04}.json` under the output directory, and emits the
`component_compose_profile` JSON event on stderr.

## Checks

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
cargo build --release
```

## Pillow parity

`scripts/rust_compose_parity.py` synthesizes a v19 output-grid job with
translucent overlap, order swaps, negative/off-canvas placements, and an
explicit empty task; composes references with Pillow (the production
compositor); runs the Rust local mode; and compares lossless RGBA and the
cwebp-encoded WebP frame bytes.

The Rust compositor blends in premultiplied RGBA with SIMD source-over, so
it is not bit-identical to Pillow's straight-alpha kernel; the comparison is
tolerance-based (`--max-channel-diff`, default 2) and prints the mismatch
statistics:

```bash
uv run --package aqw-char-renderer python scripts/rust_compose_parity.py --keep
```

## Lambda build

The Dockerfile builds inside `public.ecr.aws/lambda/provided:al2023` (the
same glibc as the runtime), pins `RUSTFLAGS=-C target-cpu=x86-64-v2` for the
AWS x86_64 instruction baseline, and ships only the static `bootstrap` plus
the same pinned `cwebp` binary the Python image uses. The image is referenced
by the isolated candidate function `aqw-char-dev-componentcompose-rust`
(reserved concurrency 1, no SQS/Step Functions wiring).