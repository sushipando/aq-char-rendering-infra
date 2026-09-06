# IIR blur traversal implementation and AWS render checks

This implements priority 1 in the [consolidated performance list](render-performance-priorities-2026-09-06.md). Deployment and AWS render measurements are left to the owner.

The vendored [IIR implementation](../services/component-raster-rust/vendor/resvg-upstream/crates/resvg/src/filter/iir_blur.rs) now visits adjacent columns together during each vertical recurrence step. Each column still receives the same downward pass, upward pass, and four-step ordering. Horizontal traversal, coefficients, precision, normalization, alpha handling, and blur dispatch are unchanged. The existing large-sigma libblur path is unchanged.

The [regression tests](../services/component-raster-rust/vendor/resvg-upstream/crates/resvg/src/filter/iir_blur_tests.rs) retain the original column traversal as an oracle. They compare both the intermediate `f64` bit patterns after filtering and final premultiplied RGBA bytes. The 512 cases cover eight dimensions, sixteen sigma pairs, and four input patterns: random premultiplied color/alpha, transparent pixels, a constant translucent field, and edge/center impulses. Cases include one-pixel dimensions, disabled axes, tiny sigmas, values near the dispatch boundary, and anisotropic values. Direct IIR tests at larger sigmas do not change which algorithm the SVG dispatcher selects.

No cache-policy bump is planned for this traversal-only change: existing pixels are intended to remain valid. Performance tests must explicitly bypass component and completed-render caches so previous rasters cannot hide the new code.

**Local validation completed**

- Both optimized IIR regression tests passed: all 512 cases preserved the floating-point results and RGBA bytes exactly.
- A fresh before/after comparison of the full 4096×2699 assembled dragon matched every decoded RGBA pixel and the complete encoded PNG bytes. Both PNGs have SHA-256 `4cc2581f32eb34953f6176b6c8715384a5166126bc0743964f15c00939e47b06`.
- All 45 raster-worker tests and all 60 enabled pipeline tests passed. Eight existing pipeline tests requiring external fixtures/tools remained ignored.
- Strict raster-worker Clippy, formatting of the changed Rust files, and whitespace checks passed.

The tests used Rust 1.98.0 on the local ARM64 Mac. The original and changed full-render probes used identical features and release settings. The isolated regression workspace was verified to contain byte-identical IIR source/tests to the actual repository. No production lockfiles were modified. Comparison builds, images, and `dragon-parity.json` are retained under `/private/tmp/aqw-iir-implementation-20260906-aneli_y9/`. These checks establish local output parity, not an AWS speedup; no deployment or AWS render submission was performed for this implementation.

**Local regression commands**

```bash
cargo test --offline --locked --manifest-path services/component-raster-rust/Cargo.toml
cargo test --offline --locked --manifest-path services/pipeline-rust/Cargo.toml
cargo clippy --offline --locked --manifest-path services/component-raster-rust/Cargo.toml --all-targets -- -D warnings
```

Cargo does not run dependency unit tests with the worker suite. The vendored resvg workspace also has a pre-existing lockfile mismatch for its existing optional libblur dependency. Run the new optimized IIR unit tests in a copied workspace to avoid changing either production lockfile or updating unrelated dependencies in the repository:

```bash
IIR_TEST_DIR="$(mktemp -d /tmp/aqw-iir-unit-tests-XXXXXX)"
cp -R services/component-raster-rust/vendor/resvg-upstream "$IIR_TEST_DIR/resvg"
cargo test --offline --release --manifest-path "$IIR_TEST_DIR/resvg/Cargo.toml" \
  -p resvg --lib --no-default-features \
  --features text,raster-images,svgz,simd-blur iir_blur
```

**AWS commands after deployment**

Run from the repository root. This submits eight normal full-workflow renders: three repeats each of frozen Annie and Dalvi, followed by Alina and Akine controls. The saved requests preserve original appearance, colors, settings, animation timing, and Map modes, including their 4096 raster / 2048 output settings. Restarts are admitted as CLI-origin jobs and never notify the original Discord destinations.

Completed-render, component, and bounds caches are disabled. Bounds also use the renderer, so their caches must not mask that path. Other cache settings remain those of each saved request; compare worker raster time separately from export/cache variability. Runs are sequential to avoid this test suite competing with itself.

The wrapper handles SSO login. Saved inputs were confirmed during the earlier audit; a fresh read-only check during implementation could not complete because the AWS SSO token had expired. Restart refuses to silently fetch a different appearance if an original snapshot is unavailable.

```bash
bash scripts/test_iir_renders.sh
```

The [operator script](../scripts/test_iir_renders.sh) contains the individual restart commands and saves each run's output with a character/repeat label. It records the raster worker's deployed code digest and memory/architecture configuration after the suite.

Keep the deployment fixed while these run. Report the results directory when finished; its logs contain the new job IDs needed to retrieve manifests, component PNGs, worker profiles, full animations, and workflow histories. A failed command stops the suite and leaves the earlier logs available.

**What to compare**

- Per-component raster times for the same Annie ground and Dalvi cape states, plus full Raster Map and workflow durations. Separate first invocations from repeated runs and inspect the distribution, not just the fastest sample.
- Decoded component RGBA, integer placement, final animation pixels/timing, and output bytes. Execution-only changes should preserve pixels; preserving PNG or WebP size alone is not a correctness check.
- Billed duration, memory, source/renderer identity, and cache-hit records. Confirm no final or component cache bypassed the work.

The historical AWS follow-up predates the measured-bounds implementation. Its timings are useful context but cannot isolate this patch's speedup if bounds or other code also changed. If a matching deployment with the completed bounds work and the old IIR traversal is still available, run the same suite before deploying this patch for a cleaner comparison. Otherwise report these as post-deployment measurements until a controlled AWS comparison is available. Local M1 duration is not an AWS forecast.
