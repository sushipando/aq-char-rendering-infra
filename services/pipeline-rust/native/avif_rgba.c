// Original straight-alpha RGBA -> one temporal AV1 sequence. Wire format v1:
// eight little-endian u32 values: width, height, physical count, quality, speed,
// lossless, threads, animated; then (duration_ms:u32, width*height*4 bytes)*count.
// The Rust caller verifies object identity and schedules before feeding stdin.
#include <avif/avif.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>

static void fail(const char *message) { fprintf(stderr, "avif-rgba: %s\n", message); exit(1); }
static void check(avifResult result) { if (result != AVIF_RESULT_OK) fail(avifResultToString(result)); }
static uint32_t number(void) {
    unsigned char b[4];
    if (fread(b, 1, 4, stdin) != 4) fail("truncated header/duration");
    return (uint32_t)b[0] | ((uint32_t)b[1] << 8) | ((uint32_t)b[2] << 16) | ((uint32_t)b[3] << 24);
}
int main(int argc, char **argv) {
    if (argc != 2) fail("usage: avif-rgba OUTPUT.avif");
    uint32_t w = number(), h = number(), count = number(), quality = number();
    uint32_t speed = number(), lossless = number(), threads = number(), animated = number();
    if (!w || !h || w > 4096 || h > 4096 || !count || count > 2000 || quality > 100 || speed > 10 ||
        lossless > 1 || !threads || threads > 6 || animated > 1 || (!animated && count != 1)) fail("invalid header");
    size_t size = (size_t)w * h * 4;
    uint8_t *pixels = malloc(size);
    uint32_t *durations = calloc(count, sizeof(uint32_t));
    avifImage *image = avifImageCreate(w, h, 8, AVIF_PIXEL_FORMAT_YUV444);
    avifEncoder *encoder = avifEncoderCreate();
    if (!pixels || !durations || !image || !encoder) fail("allocation failed");
    image->yuvRange = AVIF_RANGE_FULL;
    image->colorPrimaries = AVIF_COLOR_PRIMARIES_BT709;
    image->transferCharacteristics = AVIF_TRANSFER_CHARACTERISTICS_SRGB;
    // Identity is a reversible G/B/R plane permutation. Also use it at q=100
    // so the highest quality setting has no RGB->YUV rounding loss.
    image->matrixCoefficients = (lossless || quality == 100) ? AVIF_MATRIX_COEFFICIENTS_IDENTITY : AVIF_MATRIX_COEFFICIENTS_BT601;
    image->alphaPremultiplied = AVIF_FALSE;
    encoder->codecChoice = AVIF_CODEC_CHOICE_AOM;
    encoder->maxThreads = (int)threads;
    encoder->speed = (int)speed;
    encoder->quality = lossless ? AVIF_QUALITY_LOSSLESS : (int)quality;
    encoder->qualityAlpha = AVIF_QUALITY_LOSSLESS;
    encoder->timescale = 1000;
    encoder->repetitionCount = AVIF_REPETITION_COUNT_INFINITE;
    encoder->autoTiling = AVIF_TRUE;
    // Pinned libavif disables lagged output for sequences carrying an alpha
    // plane. RGBA conversion always supplies one, including opaque frames.
    encoder->creationTime = 1;
    encoder->modificationTime = 1;
    avifRGBImage rgb;
    avifRGBImageSetDefaults(&rgb, image);
    rgb.format = AVIF_RGB_FORMAT_RGBA;
    rgb.pixels = pixels;
    rgb.rowBytes = w * 4;
    rgb.alphaPremultiplied = AVIF_FALSE;
    uint64_t total = 0;
    for (uint32_t i = 0; i < count; ++i) {
        durations[i] = number();
        if (!durations[i]) fail("zero duration");
        total += durations[i];
        if (fread(pixels, 1, size, stdin) != size) fail("truncated RGBA frame");
        check(avifImageRGBToYUV(image, &rgb));
        check(avifEncoderAddImage(encoder, image, durations[i], animated ? AVIF_ADD_IMAGE_FLAG_NONE : AVIF_ADD_IMAGE_FLAG_SINGLE));
    }
    if (fgetc(stdin) != EOF || ferror(stdin)) fail("unexpected trailing input");
    avifRWData output = AVIF_DATA_EMPTY;
    check(avifEncoderFinish(encoder, &output));
    avifEncoderDestroy(encoder);
    avifImageDestroy(image);
    free(pixels);
    // Inspect the actual finished container before publishing. No second pixel
    // decode in production: regression tests separately verify decoded pixels.
    avifDecoder *decoder = avifDecoderCreate();
    if (!decoder) fail("decoder allocation failed");
    decoder->imageSizeLimit = 4096 * 4096;
    decoder->imageDimensionLimit = 4096;
    decoder->imageCountLimit = 2000;
    check(avifDecoderSetIOMemory(decoder, output.data, output.size));
    check(avifDecoderParse(decoder));
    if (decoder->image->width != w || decoder->image->height != h || decoder->imageCount != (int)count ||
        decoder->image->depth != 8 || decoder->image->yuvFormat != AVIF_PIXEL_FORMAT_YUV444 ||
        decoder->image->alphaPremultiplied) fail("encoded image properties mismatch");
    if (animated) {
        if (!decoder->imageSequenceTrackPresent || decoder->timescale != 1000 || decoder->durationInTimescales != total ||
            decoder->repetitionCount != AVIF_REPETITION_COUNT_INFINITE) fail("encoded animation timing/loop mismatch");
        for (uint32_t i = 0; i < count; ++i) {
            avifImageTiming timing;
            check(avifDecoderNthImageTiming(decoder, i, &timing));
            if (timing.timescale != 1000 || timing.durationInTimescales != durations[i]) fail("encoded frame duration mismatch");
        }
    }
    avifDecoderDestroy(decoder);
    FILE *file = fopen(argv[1], "wb");
    if (!file || fwrite(output.data, 1, output.size, file) != output.size || fclose(file)) fail("output write failed");
    printf("{\"width\":%u,\"height\":%u,\"physical_frame_count\":%u,\"duration_ms\":%llu,\"bytes\":%zu}\n",
           w, h, count, (unsigned long long)total, output.size);
    avifRWDataFree(&output);
    free(durations);
    return 0;
}
