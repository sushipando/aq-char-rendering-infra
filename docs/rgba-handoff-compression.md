# Lossless compression for the AVIF frame handoff

`render.rgba_compression` accepts `zstd` (default for new requests) or `none`. The CLI exposes `--rgba-compression zstd|none`. Discord's existing format selector offers AVIF with compressed or uncompressed intermediates, keeping the command at 25 options. WebP is unaffected.

PNG was already lossless. The old compose path compressed RGBA to PNG before cwebp; the initial AVIF implementation skipped PNG entirely. This option uses zstd level 1 on the same original RGBA to reduce S3 traffic without running PNG filtering or changing any pixels. AVIF color quality and lossless settings remain separate controls for the final image.

The composer reuses one zstd compressor per batch. It uploads compressed bytes only when they are smaller than the raw frame; otherwise it stores raw RGBA. Schema-2 frame records identify the actual compression, stored length/hash, and—when compressed—the original RGBA hash. Existing uncompressed records remain supported.

The finalizer checks the stored payload, keeps downloads compressed, and streams decompression into the encoder through a 64 KiB scratch buffer. It bounds the zstd window and decoded length and validates the original RGBA hash before publication. This avoids an extra full decompressed frame allocation in Rust. The native encoder still needs its own decoded RGBA/YUV buffers. Composition also still needs its raw canvas and temporarily holds both raw and compressed buffers while choosing which to upload; zstd does not guarantee lower overall peak Lambda memory.

Compression is excluded from final-render cache identity because it does not change rendered image content or metadata. Bypass the final cache when comparing transport modes. The integration test uses the same generating job ID and verifies byte-identical final AVIF files across both modes, for stills and animations. It also verifies smaller stored objects for the compressible fixture. Corrupt, truncated, wrong-checksum, and oversized decoded streams fail validation. These tests establish correctness, not an AWS speed forecast.

After the owner builds and deploys, compare these sequentially on the same equipped character:

```bash
scripts/render-character Annie --format avif --raster-size 4096 --output-size 2048 --max-frames 120 --avif-quality 70 --rgba-compression none --no-render-cache
scripts/render-character Annie --format avif --raster-size 4096 --output-size 2048 --max-frames 120 --avif-quality 70 --rgba-compression zstd --no-render-cache
```

Repeat in reverse order to reduce warm-worker effects. Compare compose `encode_ms`/`upload_ms`, finalizer `rgba_download_bytes`/`rgba_decompression_ms`, overall duration, and Lambda `Max Memory Used`. File bytes may differ in metadata because different jobs embed different generating IDs; transport compression does not alter encoded pixels. No ARM container builds or deployments were run for this change.

References: [Zstandard project](https://github.com/facebook/zstd), [Rust streaming decoder API](https://docs.rs/zstd/0.13.3/zstd/stream/read/struct.Decoder.html).
