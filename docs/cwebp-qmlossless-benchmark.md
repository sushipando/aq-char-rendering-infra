# cwebp q/m/lossless comparison — alina, 120 frames @ 2048px

Benchmark of the `q` (quality), `m` (method), and `lossless` cwebp toggles
added to the character-render pipeline. Each combo was rendered through the
real SQS workflow with `scripts/benchmark_cwebp_grid.py` and aggregated with
`scripts/aggregate_cwebp_grid.py`.

- Character: **alina**
- Frames: **120**, raster **4096** → output **2048** (output-grid space)
- Final output: single animated WebP (webpmux of 120 cwebp-encoded frames)
- Recorded: **2026-09-03**, dev stack, Rust SIMD compositor deployed
  (`aqw-char-dev-componentcompose-rust`)

## Column definitions

| Column | Meaning |
|---|---|
| `q` | cwebp `-q` quality factor (lossy only; ignored under `-lossless`) |
| `m` | cwebp `-m` compression method 0..6 (higher = slower, smaller) |
| `lossless` | cwebp `-lossless 1` flag |
| `bytes` | final animated WebP size served from CloudFront |
| `full_render_s` | wall time from queued → job SUCCEEDED (job table) |
| `compose_stage_s` | `ComposeComponentFrameChunk` map wall time (12 parallel batches of 10 frames) |
| `encode_s` | total cwebp encode time summed over the 12 batches (CloudWatch `component_compose_profile`) |
| `batches` | number of compose batches contributing to the sums |
| `url` | public CloudFront asset link |

## Results

| q | m | lossless | bytes | full_render_s | compose_stage_s | encode_s | batches | url |
|---|---|---|---|---|---|---|---|---|
| 75 | 4 | null | 13,984,484 | 28.2 | 9.1 | 55.84 | 12 | https://d3uyqw783yqoa2.cloudfront.net/renders/v19/q75/2048/46/463838291b8f4947f781c34acba361e6859ddfea342ec3a211c28806ecdd9639.webp |
| 82 | 4 | null | 15,675,938 | 32.8 | 8.9 | 56.29 | 12 | https://d3uyqw783yqoa2.cloudfront.net/renders/v19/q82/2048/f9/f990779c3339dfca1a5f57fdb8b60c36a07b2df804a06d9e2ea79cf05e828c1d.webp |
| 85 | 4 | null | 16,606,978 | 22.7 | 9.2 | 56.55 | 12 | https://d3uyqw783yqoa2.cloudfront.net/renders/v19/q85/2048/69/69f77bfe088be201f12dd1c7d017e22210edd306f2477ab60eb185643f7790ca.webp |
| 70 | 1 | null | 16,111,708 | 20.2 | 6.1 | 36.40 | 12 | https://d3uyqw783yqoa2.cloudfront.net/renders/v19/q70/2048/ee/ee51ed134cc6179789a710d3c9d4a0d765e31430e07c22a1440465003427fe5e.webp |
| 70 | 2 | null | 13,812,460 | 21.2 | 6.9 | 38.92 | 12 | https://d3uyqw783yqoa2.cloudfront.net/renders/v19/q70/2048/34/348f90e4b20fb4a54ce06f6a7171470bce6af388963713af25c699753fc0cb34.webp |
| 75 | 4 | lossless | 37,864,302 | 37.1 | 19.7 | 138.78 | 12 | https://d3uyqw783yqoa2.cloudfront.net/renders/v19/q75/2048/c4/c4d019731aceee18a4fbc102ffd71d875ca6c285e2a3842dde437e54ad926bf0.webp |
| 0 | 0 | lossless | 50,490,872 | 20.9 | 6.5 | 25.84 | 12 | https://d3uyqw783yqoa2.cloudfront.net/renders/v19/q0/2048/46/46da2533addb82db30a80e7c940a3967f9de113a4edc5e9cc243f0d0d2dacdaf.webp |
| 20 | 1 | lossless | 42,240,204 | 25.7 | 12.4 | 83.40 | 12 | https://d3uyqw783yqoa2.cloudfront.net/renders/v19/q20/2048/4c/4c18227ed12367e2a23a0dba1eb8b593ba3ea5c911b7c72f4992e816ba377f45.webp |
| 25 | 2 | lossless | 38,084,056 | 30.3 | 19.0 | 122.23 | 12 | https://d3uyqw783yqoa2.cloudfront.net/renders/v19/q25/2048/8b/8bf5365e92882825a78d0952a6d6a685e9160a8cc7388eaf439c7211a835baf1.webp |
| 30 | 3 | lossless | 37,932,576 | 31.1 | 19.4 | 123.51 | 12 | https://d3uyqw783yqoa2.cloudfront.net/renders/v19/q30/2048/61/61331d95e6195161c531634a70ab9e0d06825083f1b7c26920b6024e3cdf2b4f.webp |
| 50 | 3 | lossless | 37,807,744 | 36.4 | 19.4 | 127.82 | 12 | https://d3uyqw783yqoa2.cloudfront.net/renders/v19/q50/2048/ba/ba48eb05b9b0d19b214ed1913c78e8d5147d3b3c3e9f337a942e3581cb253839.webp |
| 60 | 4 | lossless | 37,815,274 | 37.6 | 20.7 | 138.74 | 12 | https://d3uyqw783yqoa2.cloudfront.net/renders/v19/q60/2048/7f/7fcaeb494603a42f9d7005a9ecca7628c319da454141b865367555470ac09ba7.webp |

## Takeaways

- **Lossy best size/quality:** `q70 / m2` → 13.8 MB (smaller than the current
  default `q85 / m4` at 16.6 MB) with ~39 s encode and ~6.9 s compose stage.
- **Fastest lossy encode:** `q70 / m1` → 36.4 s, but 16.1 MB.
- **Lossless is expensive:** ~2.3–2.7× the file size of `q85/m4`
  (37–50 MB) and 2–5× encode time.
  - `q25–50` all converge to ≈37.8 MB; `q30–50` gives the best size/time ratio.
  - `q0 / m0` lossless is fastest (25.8 s) but largest (50.5 MB) since the
    quality factor is ignored under `-lossless`.
- `compose_stage_s` tracks encode (batches compose in parallel), so cwebp
  parameters dominate the `ComposeComponentFrameChunk` wall time.

## Jobs

| combo | job_id |
|---|---|
| 75/4/lossy | `b54d91a0-34e1-4536-89cd-f69d34aa8e68` |
| 82/4/lossy | `0879ee72-fe11-4104-b07a-2a394fddb16e` |
| 85/4/lossy | `b68e6b4e-459f-4dcb-ada0-9c0e3c977a92` |
| 70/1/lossy | `bc8c3551-172e-4bff-88e5-886edf52c727` |
| 70/2/lossy | `3179e5fd-7eff-436e-9bb5-3f9d7020d083` |
| 75/4/lossless | `ab493c51-df92-404e-8de0-292e63205823` |
| 0/0/lossless | `5d5ff5f5-b071-412b-b4c8-a041a5f13c65` |
| 20/1/lossless | `ad4c9b09-a006-4c09-ad28-3034d8e9ce4c` |
| 25/2/lossless | `1c0656f8-107a-409e-8f7a-53763ca03e2d` |
| 30/3/lossless | `0b71d82d-d569-4ee8-8d7c-6a14c747619c` |
| 50/3/lossless | `31a128e5-8b56-416d-9e70-21a0ea984756` |
| 60/4/lossless | `e623abf2-e276-4294-a8da-21caac5a0466` |

## Reproduction

```bash
# queue + benchmark one combo grid through the real pipeline
AWS_PROFILE=aqw-char-dev AWS_DEFAULT_REGION=us-west-2 \
  uv run --package aqw-char-renderer python scripts/benchmark_cwebp_grid.py \
    --username alina --max-frames 120 --output-size 2048

# aggregate job-table + CloudWatch timing into the table above
uv run --package aqw-char-renderer python scripts/aggregate_cwebp_grid.py \
    --grid-log /path/to/grid.log
```