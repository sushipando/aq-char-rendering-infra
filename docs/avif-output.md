# Optional AVIF output

Idea 2 uses the existing compose Map and finalizer Lambda. The request selects `render.output_format: "webp" | "avif"`; WebP remains the default. No new workflow nodes, GPU, EC2 fleet, or capacity request is required. ARM image builds and deployment are reserved for the repository owner.

## Encoding and quality

| Setting | Default | Effect |
| --- | --- | --- |
| `output_format` | `webp` | Selects final format and intermediate representation. |
| `avif_quality` | `70` | Integer 0–100; higher retains more detail and generally increases bytes. |
| `avif_speed` | `8` | Integer 0–10; higher trades compression efficiency for faster encoding. |
| `webp_lossless` | false/null | Existing toggle applies to either format. AVIF ignores lossy quality when enabled. |
| `webp_quality`, `webp_method` | 85, 4 | Continue controlling WebP. |

AVIF uses one libaom sequence encoder, 8-bit full-range 4:4:4 color, exact alpha, and straight RGBA input. Lossless mode and quality 100 use the reversible identity color matrix to avoid RGB/YUV rounding loss. Lossless means exact **composed output RGBA**, after the existing rasterization, compositing, and downsampling. It does not undo those earlier operations. Quality 70 is a starting point for deployment comparisons, not a promise of near-lossless pixels or a particular file size.

The pinned native helper uses libavif 1.4.2 and its pinned libaom 3.14.1 dependency, two threads, and automatic tiling. It receives frames over stdin. Every sequence includes an alpha plane; this pinned libavif implementation disables libaom lagged output for alpha sequences. It compresses frames temporally, instead of encoding independent AVIF stills. CPU-only encoding can still be substantial; the finalizer allows 840 seconds for encoding inside a 900-second Lambda timeout. Its memory allocation remains 3008 MiB. Increasing the timeout does not reserve extra CPU or incur idle Lambda charges.

## Frame handoff and validation

* WebP follows the existing per-composition PNG → cwebp → WebP-mux path.
* AVIF composition skips both PNG compression and cwebp, uploading tightly packed original RGBA8 once per unique composition. Schema-2 records use `rgba_key`, never `webp_key`. Logical frames still share composition objects.
* Finalization verifies every logical frame's identity, dimensions, placement, byte count, and timing. It checks downloaded RGBA checksums and lengths. Missing, duplicated, swapped-format, or conflicting records fail before publication.
* At most two raw downloads are in flight; pipe backpressure bounds input buffering. The finalizer does not accumulate the raw animation in RAM or `/tmp`. Nonadjacent reuse may download an object again. Adjacent repetitions of the same object merge, except tiny durations. Fully static animations retain two samples because libavif otherwise emits a still container and loses animation timing.
* The helper checks the finished container's dimensions, color layout, sample count, durations, and infinite loop before reporting success. Only then does Rust publish `image/avif` and final-cache metadata. Decoder regression tests independently check actual pixels.

Raw intermediates are larger: one 2048×2048 RGBA frame is 16 MiB. They stay in the work bucket under its existing lifecycle rules; they are not the Discord attachment. This trades intermediate S3 traffic for less compose CPU and avoids transcoding already-lossy WebP. Output size is not capped at 10 MB, and true lossless AVIF can exceed that substantially. AWS latency, memory use, transferred bytes, and final bytes must be measured together. Mac correctness tests are not AWS performance forecasts. The attempted local ARM64 image build was stopped at the owner’s request; container compilation and runtime packaging remain for the owner to verify.

The final key uses `.avif`, the correct content type, and the existing render cache-control/content-disposition behavior. Format, quality, speed, lossless choice, and AVIF encoder policy participate in final identity. Vector/bounds/component reuse remains independent of the final encoding path. Adding normalized output settings changes final-cache identities for this release.

## Discord companion change

`aq-image-search` has a companion commit: `/render output_format:AVIF quality:70` selects AVIF, and the existing `webp_lossless` toggle applies to it. WebP defaults to quality 85; AVIF defaults to 70. The API uses integer AVIF quality, so fractional Discord values are rounded for AVIF.

The command already had 25 options. Its single-choice `raster_backend:resvg` control is replaced with `output_format`; it still submits patched resvg. The displayed `webp_quality` option becomes `quality`, while `webp_method` remains WebP-specific. Delivery continues through the existing result URL. Deploy the infrastructure before restarting/syncing the updated bot. Actual Discord animated-AVIF preview behavior still needs checking in the test server. Both formats now carry embedded character/item metadata and the generating job ID; see [render metadata](render-file-metadata.md).

## Deployment render checks

The owner deploys using the existing repository deployment procedure. Then run these commands sequentially with the same equipped character. They preserve upstream caches while bypassing the final cache, so the encoding comparisons do real work.

```bash
scripts/render-character Annie --format webp --raster-size 4096 --output-size 2048 --max-frames 120 --webp-quality 85 --no-render-cache
scripts/render-character Annie --format avif --raster-size 4096 --output-size 2048 --max-frames 120 --avif-quality 70 --avif-speed 8 --no-render-cache
scripts/render-character Annie --format avif --raster-size 4096 --output-size 2048 --max-frames 120 --avif-quality 85 --avif-speed 8 --no-render-cache
scripts/render-character Annie --format avif --raster-size 4096 --output-size 2048 --max-frames 120 --lossless --no-render-cache
```

Check the still path and final-cache hit:

```bash
scripts/render-character Annie --format avif --raster-size 4096 --output-size 2048 --no-complete-loop --lossless --no-render-cache
scripts/render-character Annie --format avif --raster-size 4096 --output-size 2048 --max-frames 120 --avif-quality 70 --avif-speed 8
```

Exercise the first idea with a real FFDec export, separately from the encoding comparison:

```bash
scripts/render-character Annie --format webp --raster-size 4096 --output-size 2048 --max-frames 120 --no-render-cache --no-vector-cache
```

Repeat the WebP/AVIF quality-70 pair for the equipped character containing your problematic large asset. Report job IDs, final URLs/bytes, and completion status. Compare compose/finalize profile durations and Lambda `Max Memory Used`, then inspect fine lines, gradients, transparent edges, timing, and looping. Run the same settings in `/render` to check Discord playback. The CLI commands remain silent on Discord.

## Local validation

Native pixel/timing tests need Pillow AVIF support and a helper built against the pinned libraries:

```bash
cc -O3 -Wall -Wextra -Werror services/pipeline-rust/native/avif_rgba.c $(pkg-config --cflags --libs libavif) -o /tmp/aqw-avif-rgba
CHAR_RENDER_AVIF_RGBA=/tmp/aqw-avif-rgba .venv/bin/python -m pytest services/pipeline-rust/native/test_avif_rgba.py -q
CHAR_RENDER_AVIF_RGBA=/tmp/aqw-avif-rgba AQW_TEST_PYTHON="$PWD/.venv/bin/python" cargo test --offline --locked --manifest-path services/pipeline-rust/Cargo.toml -j 2 full_rust_pipeline_encodes_and_validates_avif -- --ignored
```

Native tests cover exact lossless RGBA (including RGB under transparent pixels), opaque-to-transparent animation, lossless alpha at quality 0 and 70, odd dimensions, stills, repeated frames, and truncated input rejection. The Rust integration test exercises real raster → raw compose → AVIF finalization and compares decoded output against the original composition objects, with a nonexistent cwebp binary.

Sources for the encoder behavior: [libavif public API](https://github.com/AOMediaCodec/libavif/blob/v1.4.2/include/avif/avif.h), [alpha sequence and lag handling](https://github.com/AOMediaCodec/libavif/blob/v1.4.2/src/write.c), [libaom configuration](https://github.com/AOMediaCodec/libavif/blob/v1.4.2/src/codec_aom.c).

Validation completed locally: 71 regular Rust pipeline tests, 50 compose tests, WebP and AVIF end-to-end integration tests, 3 native encoder/decoder tests, 60 Python contract/notification tests, 16 operator-script tests, 21 bot/command tests, 17 infrastructure tests, and TypeScript compilation. The AVIF integration also verifies that corrupt RGBA cannot publish an image or final-cache metadata. Tests requiring a real FFDec corpus were not run for these changes. No AWS render benchmark, ARM image build completion, or deployment is claimed.
