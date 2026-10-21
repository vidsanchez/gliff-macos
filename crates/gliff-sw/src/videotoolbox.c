// Apple ABI stays in C, compiled against SDK headers. No callbacks enter Rust.
#include <CoreFoundation/CoreFoundation.h>
#include <CoreMedia/CoreMedia.h>
#include <CoreVideo/CoreVideo.h>
#include <VideoToolbox/VideoToolbox.h>
#include <math.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

#define LIMIT 4096u
#define AU_LIMIT (64u * 1024u * 1024u)
#define INVALID (-200001)
#define NO_PICTURE (-200002)
typedef struct {
    VTDecompressionSessionRef session;
    CMVideoFormatDescriptionRef format;
    size_t width, height, encoded_width, encoded_height, x, y;
    int64_t sequence;
} GliffDecoder;
typedef struct {
    GliffDecoder *decoder;
    uint8_t *pixels;
    size_t capacity;
    unsigned calls;
    OSStatus status;
} Output;

// Checked copy shared by the callback and synthetic stride/capacity tests.
int32_t gliff_copy_bgra(const uint8_t *src, size_t src_len, size_t stride,
    size_t source_width, size_t source_height, size_t x, size_t y,
    size_t width, size_t height, uint8_t *dst, size_t capacity) {
    if (!src || !dst || !width || !height || !source_width || !source_height ||
        source_width > LIMIT || source_height > LIMIT || width > source_width ||
        height > source_height || x > source_width - width || y > source_height - height ||
        stride < source_width * 4 || stride > SIZE_MAX / source_height ||
        src_len < stride * source_height || capacity < width * height * 4) return INVALID;
    for (size_t row = 0; row < height; row++)
        memcpy(dst + row * width * 4, src + (y + row) * stride + x * 4, width * 4);
    return 0;
}
static void output(void *unused, void *context, OSStatus status, VTDecodeInfoFlags flags,
    CVImageBufferRef image, CMTime pts, CMTime duration) {
    (void)unused; (void)pts; (void)duration;
    Output *out = context;
    if (!out) return;
    out->calls++;
    out->status = status;
    if (status) return;
    if (out->calls != 1 || !image || (flags & kVTDecodeInfo_FrameDropped)) {
        out->status = NO_PICTURE; return;
    }
    if (CVPixelBufferIsPlanar(image) || CVPixelBufferGetPixelFormatType(image) != kCVPixelFormatType_32BGRA) {
        out->status = INVALID; return;
    }
    GliffDecoder *d = out->decoder;
    size_t width = CVPixelBufferGetWidth(image), height = CVPixelBufferGetHeight(image);
    size_t x = 0, y = 0;
    if (width == d->encoded_width && height == d->encoded_height) { x = d->x; y = d->y; }
    else if (width != d->width || height != d->height) { out->status = INVALID; return; }
    CVReturn lock = CVPixelBufferLockBaseAddress(image, kCVPixelBufferLock_ReadOnly);
    if (lock) { out->status = lock; return; }
    out->status = gliff_copy_bgra(CVPixelBufferGetBaseAddress(image), CVPixelBufferGetDataSize(image),
        CVPixelBufferGetBytesPerRow(image), width, height, x, y, d->width, d->height,
        out->pixels, out->capacity);
    CVReturn unlock = CVPixelBufferUnlockBaseAddress(image, kCVPixelBufferLock_ReadOnly);
    if (!out->status) out->status = unlock;
}
void gliff_vt_destroy(GliffDecoder *d) {
    if (!d) return;
    // flags=0 means decode has completed before we can enter this function.
    if (d->session) { VTDecompressionSessionInvalidate(d->session); CFRelease(d->session); }
    if (d->format) CFRelease(d->format);
    free(d);
}
int32_t gliff_vt_create(const uint8_t *sps, size_t sps_len, const uint8_t *pps,
    size_t pps_len, uint32_t width, uint32_t height, GliffDecoder **result) {
    if (!result) return INVALID;
    *result = NULL;
    if (!sps || !pps || !sps_len || !pps_len || sps_len > 65536 || pps_len > 65536 ||
        !width || !height || width > LIMIT || height > LIMIT) return INVALID;
    GliffDecoder *d = calloc(1, sizeof(*d));
    if (!d) return memFullErr;
    const uint8_t *sets[] = {sps, pps};
    size_t sizes[] = {sps_len, pps_len};
    OSStatus status = CMVideoFormatDescriptionCreateFromH264ParameterSets(NULL, 2, sets, sizes, 4, &d->format);
    if (status) goto fail;
    CMVideoDimensions encoded = CMVideoFormatDescriptionGetDimensions(d->format);
    CGRect clean = CMVideoFormatDescriptionGetCleanAperture(d->format, true);
    if (encoded.width <= 0 || encoded.height <= 0 || encoded.width > (int32_t)LIMIT || encoded.height > (int32_t)LIMIT ||
        !isfinite(clean.origin.x) || !isfinite(clean.origin.y) || clean.origin.x < 0 || clean.origin.y < 0 ||
        clean.origin.x > encoded.width || clean.origin.y > encoded.height ||
        clean.size.width != width || clean.size.height != height) { status = INVALID; goto fail; }
    d->x = (size_t)clean.origin.x; d->y = (size_t)clean.origin.y;
    d->width = width; d->height = height;
    d->encoded_width = encoded.width; d->encoded_height = encoded.height;
    if (d->x != clean.origin.x || d->y != clean.origin.y || width > d->encoded_width - d->x ||
        height > d->encoded_height - d->y) { status = INVALID; goto fail; }
    int32_t pixel_format = kCVPixelFormatType_32BGRA;
    CFNumberRef number = CFNumberCreate(NULL, kCFNumberSInt32Type, &pixel_format);
    if (!number) { status = memFullErr; goto fail; }
    const void *keys[] = { kCVPixelBufferPixelFormatTypeKey };
    const void *values[] = { number };
    CFDictionaryRef attributes = CFDictionaryCreate(NULL, keys, values, 1,
        &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks);
    CFRelease(number);
    if (!attributes) { status = memFullErr; goto fail; }
    VTDecompressionOutputCallbackRecord callback = { output, NULL };
    status = VTDecompressionSessionCreate(NULL, d->format, NULL, attributes, &callback, &d->session);
    CFRelease(attributes);
    if (status) goto fail;
    *result = d;
    return 0;
fail:
    gliff_vt_destroy(d);
    return status;
}
int32_t gliff_vt_decode(GliffDecoder *d, const uint8_t *avcc, size_t length,
    uint8_t *pixels, size_t capacity) {
    if (!d || !d->session || !avcc || !length || length > AU_LIMIT || !pixels ||
        capacity < d->width * d->height * 4 || d->sequence == INT64_MAX) return INVALID;
    CMBlockBufferRef block = NULL;
    CMSampleBufferRef sample = NULL;
    OSStatus status = CMBlockBufferCreateWithMemoryBlock(NULL, NULL, length, NULL, NULL,
        0, length, kCMBlockBufferAssureMemoryNowFlag, &block);
    if (status) goto done;
    status = CMBlockBufferReplaceDataBytes(avcc, block, 0, length);
    if (status) goto done;
    CMSampleTimingInfo timing = { kCMTimeInvalid, CMTimeMake(d->sequence++, 60), kCMTimeInvalid };
    status = CMSampleBufferCreateReady(NULL, block, d->format, 1, 1, &timing, 1, &length, &sample);
    if (status) goto done;
    Output out = { d, pixels, capacity, 0, NO_PICTURE };
    // CRITICAL: no asynchronous or temporal flags. Apple guarantees callback
    // completion before return; stack Output and borrowed Rust pixels stay valid.
    status = VTDecompressionSessionDecodeFrame(d->session, sample, 0, &out, NULL);
    if (!status) status = out.calls == 1 ? out.status : NO_PICTURE;
done:
    if (sample) CFRelease(sample);
    if (block) CFRelease(block);
    return status;
}
