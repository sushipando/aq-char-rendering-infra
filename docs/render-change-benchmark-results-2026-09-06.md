# AWS results: changes after the IIR baseline

Evidence: `tmp/render-change-benchmark/comparison.json`, `REPORT.md`, per-job `evidence/*/summary.json` and `profiles.json`, and configuration snapshots. These are AWS arm64 runs, not M1 extrapolations. All 26 candidates succeeded; no final-cache hits, recorded task failures, or harness comparison warnings. The recorded Lambda code hashes, architecture and memory settings were consistent across candidate configuration snapshots. Historical code provenance is not established by the current configuration capture.

## Matched WebP results

Three new samples per workload; historical Annie and Dalvi have three samples each, others one. Times are whole Step Functions execution wall time.

| Workload | Historical median s | New median s | Observed reduction | New MiB |
|---|---:|---:|---:|---:|
| Akine | 47.15 | 22.73 | 51.8% | 39.058 |
| Alina | 17.48 | 9.56 | 45.3% | 3.981 |
| Dalvi | 62.92 | 58.15 | 7.6% | 62.350 |
| ordinaryboy | 26.78 | 24.38 | 9.0% | 33.530 |
| Queen Annie 012 | 56.70 | 51.86 | 8.5% | 21.525 |

Do not interpret these percentages as isolated renderer speedups. Akine's export map fell from 24.99 s to a 0.48 s median; Alina's fell from 9.57 s to 0.28 s. This is consistent with export-cache warmth differing despite matching cache flags. Dalvi's historical range (59.29–65.47 s) overlaps the new range (52.60–63.99 s); Annie's historical range (51.01–71.98 s) includes the new range (51.67–53.86 s). More tightly controlled comparisons are needed to attribute modest improvements.

WebP grew by only 1,362–1,654 bytes relative to the first historical sample for each workload. That is consistent with added metadata overhead, but this size comparison alone does not prove identical pixels or valid metadata.

## AVIF and zstd

One sample per mode per workload, quality 70, speed 8; no lossless samples were run.

| Workload | New WebP median s / MiB | AVIF raw s / MiB | AVIF zstd s / MiB |
|---|---|---|---|
| Dalvi | 58.15 / 62.350 | 71.89 / 13.937 | 70.45 / 13.937 |
| ordinaryboy | 24.38 / 33.530 | 37.05 / 3.794 | 37.18 / 3.794 |
| Annie | 51.86 / 21.525 | 57.55 / 0.659 | 63.73 / 0.659 |

AVIF solves much of the file-size problem, but at these settings adds workflow time. Dalvi remains above either a 10 MB or 10 MiB budget. Quality 70 cannot be called near-lossless from these measurements; visual inspection is outstanding.

Zstd is worth retaining as the default handoff. Across the captured compose profiles, its added encoding work was outweighed by lower upload time:

| Workload | Median compose encode ms, raw → zstd | Median compose upload ms, raw → zstd | Median compose total ms, raw → zstd |
|---|---|---|---|
| Dalvi | 129.6 → 197.1 | 612.1 → 217.1 | 1073.5 → 735.9 |
| ordinaryboy | 86.2 → 135.3 | 534.0 → 234.3 | 854.0 → 613.6 |
| Annie | 32.3 → 51.6 | 179.7 → 87.2 | 415.2 → 352.6 |

These are medians of captured parallel compose tasks, not whole-workflow savings. Some logs were absent at collection time, including several finalizer profiles; therefore do not treat the log set as a complete accounting of invocations or bytes.

Annie's zstd workflow was 6.18 s slower than raw, but its raster map alone was 5.40 s slower (41.96 vs 36.56 s); the compose map was slightly faster (0.91 vs 0.96 s). That result does not establish a 6 s compression penalty. Finalization was 13.22 vs 12.74 s.

AVIF finalization is the next encoding bottleneck: roughly 25–26 s for Dalvi, 15–16 s for ordinaryboy, and 13 s for Annie, versus roughly 0.8–1.7 s for their matched WebP finalization. Zstd cannot remove the AVIF encoder's work. Finalization includes transfers and other work; these are not isolated encoder CPU times.

Raw and zstd target the same final object key, intentionally excluding transport settings from the cache identity. Final caching was disabled, so each job did run, but the later upload replaces the object at that key. Their shared output URL is not a preserved pair for pixel comparison. Repeated matched WebPs similarly share keys. Preserve separate downloads or S3 versions in a future pixel/metadata experiment.

## Raster hotspots remain decisive

The matched logs identify concrete targets:

- Annie's `ground` component spends **40.99–41.31 s inside rasterization** in the three matched runs. It has 101 filters, a 501,707-byte SVG and 2,439,486 raster pixels. Its entire task takes about 41.6–41.9 s. This is a clear compute hotspot; reducing orchestration or upload latency will barely change that task.
- Dalvi's slowest `cape` profiles spend **about 26.4–26.5 s in rasterization**, with 20–22 filters and 1,834,289 raster pixels. The raster map median is 36.95 s versus a slowest-task median of 27.19 s, so map scheduling/waves and other tasks also matter. These measurements do not yet identify which filter primitive consumes the CPU.

Prioritize those exact SVGs for filter traversal/allocation profiling before selecting another blur algorithm or replacing resvg. The evidence supports targeting filtered raster work, but does not prove copying, color matrices or blurs individually dominate.

## Fresh export results

Export-cold workflow times were Akine 51.97 s, Alina 14.12 s, Dalvi 52.44 s, ordinaryboy 23.65 s and Annie 56.32 s. All succeeded with matching recorded geometry/timing/source fingerprints.

Akine's fresh export map took **34.39 s**, longer than its historical 24.99 s export map. Investigate this asset's per-source export/normalization work; the apparent 52% matched speedup does not demonstrate faster cold exports. The other fresh export maps were 5.58–9.55 s. Each cold variant has one sample, and historical cache state was not controlled, so this is a target for investigation rather than proof of a regression.

## Next actions

1. Keep zstd enabled: observed compose/upload times improve without increasing final image size. Peak memory savings were not measured.
2. Profile Annie ground and Dalvi cape at the filter-primitive level on AWS. These are the strongest remaining raster targets; keep resolution and pixels fixed when evaluating improvements.
3. Investigate Akine's fresh export path separately from cached exports.
4. Evaluate AVIF speed/quality settings on the same RGBA input, concentrating on Dalvi's remaining size excess and the 13–26 s finalization cost. Retain representative frame crops for quality comparison; do not trade away crisp lines based only on byte counts.

No additional AWS jobs, builds or deployments were launched for this review. No application code was changed. The analysis does not establish losslessness, validate embedded metadata, or estimate billed cost from workflow wall times.
