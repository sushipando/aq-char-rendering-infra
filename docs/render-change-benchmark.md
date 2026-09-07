# Benchmark the changes after IIR

Use `scripts/benchmark-render-changes` from the repository root. It never builds or deploys. Without `--run`, it only reads historical AWS evidence and writes a local plan/report. You run the render commands after deploying.

The captured baseline is in `tmp/render-change-benchmark/`. It includes job `6fd4e305-3ff1-4c5f-a253-65b7e5530242` (ordinaryboy) and the eight render jobs in `/private/tmp/aqw-iir-render-checks-km9USb`. This is the actual directory name; the supplied path had an extra `s`.

Those nine post-IIR samples represent **five distinct workloads**: Akine, Alina, Dalvi, ordinaryboy, and Queen Annie 012 (`--only annie` works). Annie and Dalvi each have three historical repetitions. The script groups by normalized request, including appearance and cache policy, rather than treating repetitions as additional workloads. It retains all historical timings and reports their median and range. This compares changes since those IIR test runs; it does not isolate the effect of IIR itself.

## Commands after your deployment

First run the matched comparison: **15 sequential renders**, three per workload. It preserves the historical appearance, resolution, frame limit, encoding settings and upstream cache flags. Every candidate disables final-result caching.

```bash
scripts/benchmark-render-changes --run --suite matched --rounds 3 --logs
```

Then explicitly exercise fresh vector exports: **five renders**. This disables vector, bounds and component caches, preserving each workload's animation-cache policy. Historical ordinaryboy already had every cache disabled; the other historical workloads allowed animation and vector caches.

```bash
scripts/benchmark-render-changes --run --suite exports --rounds 1 --logs
```

Compare AVIF raw and zstd handoffs on three workloads: **six renders**, using quality 70 and speed 8. These preserve the historical raster/output sizes and upstream cache policy. Both modes use the new original-RGBA path; this tests the transport toggle, not old WebP intermediates versus RGBA in isolation.

```bash
scripts/benchmark-render-changes --run --suite codecs --rounds 1 \
  --only annie,dalvi,ordinaryboy --logs
```

Optional lossless AVIF adds one render per selected workload. Running the following after the codec command above reuses its completed raw/zstd runs and submits **three additional renders**:

```bash
scripts/benchmark-render-changes --run --suite codecs --rounds 1 \
  --only annie,dalvi,ordinaryboy --include-lossless --logs
```

Quality 70 is an experimental setting, not a near-lossless guarantee. Use `--avif-quality` and `--avif-speed` to explore alternatives; distinct quality/speed settings receive separate run identities and report rows. Lossless refers to encoding the rendered frames, not undoing rasterization or resizing.

## Capture, resume and compare

The baseline has already been captured read-only. To capture in another directory, or preview a plan, omit `--run`:

```bash
scripts/benchmark-render-changes --output-dir tmp/render-change-benchmark-next --logs
```

Use **one output directory per deployed version**. Repeating a command with the same directory resumes its journal, skips completed jobs, and does not create another sample for an already completed round. Increase `--rounds` to add repetitions. A timeout stops the sequence; rerun the same command to resume the recorded job. Do not run two harness processes against the same directory simultaneously.

For a later deployment, choose a new directory. The script can recapture the same historical jobs while AWS history/artifacts remain available. Alternatively copy `baselines.json`, `evidence/`, and `operator-baselines/` from the original bundle into a fresh directory, excluding `runs/`. Baseline capture configuration is explicitly labeled as configuration at capture time, not proof of the code used by historical executions.

The default AWS profile is `aqw-char-dev`, region `us-west-2`, outputs file `cdk-outputs.dev.json`. Flags can override these. Frozen bundles are checked against the configured state machine. No live character lookup is substituted for the stored appearance; missing historical appearance evidence fails the capture.

Read `REPORT.md` for execution wall times, file sizes in MiB, and matched-workflow time ratios (below 1 is faster). Reports cover the latest selected workloads; rerun without `--only` and without `--run` to regenerate the report across all five. `comparison.json` and `evidence/<job>/` contain requests, full Step Functions histories, per-state distributions, source fingerprints, framing/timing, and optional Lambda profile events. Files remain in the output directory for sharing after the commands finish.

Dimension, frame-count, duration, source, framing and final-cache differences produce warnings. The report suppresses matched speed ratios when those comparisons differ. Changed codec/cache settings are shown as separate experiments without a matched speedup claim. Historical cache flags do not prove identical actual cache warmth; retries, cold starts, other traffic and deployment configuration can affect timings. Three samples describe observed variability, not statistical certainty.

These are AWS measurements, not projections from M1 macOS. Parallel state durations must not be summed as wall time. Lambda peak memory, billed GB-seconds, pixel-quality scores and embedded metadata validity are not inferred by this harness. It records output URLs for subsequent visual/metadata checks; it does not automatically download and decode the images. Raw/zstd should preserve identical pixels, but file bytes can differ because fresh job IDs are embedded in metadata. Check visual quality and Discord's actual upload limit separately from speed and size.
