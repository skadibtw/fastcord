#include <CoreFoundation/CoreFoundation.h>
#include <CoreMedia/CoreMedia.h>
#include <CoreVideo/CoreVideo.h>
#include <VideoToolbox/VideoToolbox.h>
#include <stdint.h>
#include <stdbool.h>
#include <stdlib.h>
#include <string.h>

#define FC_POOL_LIMIT 3
#define FC_START_CODE_SIZE 4

typedef void (*FcEncoded)(void *, const uint8_t *, size_t, uint64_t, int);
typedef void (*FcDecoded)(void *, const uint8_t *, size_t, size_t, const uint8_t *, size_t, size_t, size_t, size_t, uint64_t);

typedef struct {
    VTCompressionSessionRef session;
    CVPixelBufferPoolRef session_pool;
    CFDictionaryRef allocation_attributes;
    uint32_t width, height;
    FcEncoded callback;
    void *callback_context;
    OSStatus callback_status;
} FcEncoder;

typedef struct {
    VTDecompressionSessionRef session;
    CMVideoFormatDescriptionRef format;
    uint8_t *sps, *pps;
    size_t sps_size, pps_size;
    uint32_t width, height;
    int hardware;
    int allow_software_fallback;
    FcDecoded callback;
    void *callback_context;
    OSStatus callback_status;
} FcDecoder;

static CFNumberRef number_i32(int32_t value) {
    return CFNumberCreate(kCFAllocatorDefault, kCFNumberSInt32Type, &value);
}

static CFDictionaryRef pixel_attributes(uint32_t width, uint32_t height) {
    int32_t format = kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange;
    CFNumberRef w = number_i32((int32_t)width), h = number_i32((int32_t)height);
    CFNumberRef f = number_i32(format);
    const void *surface_value = CFDictionaryCreate(kCFAllocatorDefault, NULL, NULL, 0, &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks);
    const void *keys[] = { kCVPixelBufferPixelFormatTypeKey, kCVPixelBufferWidthKey, kCVPixelBufferHeightKey, kCVPixelBufferIOSurfacePropertiesKey };
    const void *values[] = { f, w, h, surface_value };
    CFDictionaryRef attrs = CFDictionaryCreate(kCFAllocatorDefault, keys, values, 4, &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks);
    CFRelease(w); CFRelease(h); CFRelease(f); CFRelease(surface_value);
    return attrs;
}

static int using_hardware(VTSessionRef session, CFStringRef key) {
    CFTypeRef value = NULL;
    if (VTSessionCopyProperty(session, key, kCFAllocatorDefault, &value) != noErr || value == NULL) return 0;
    int result = CFGetTypeID(value) == CFBooleanGetTypeID() && CFBooleanGetValue((CFBooleanRef)value);
    CFRelease(value);
    return result;
}

static void compression_output(void *refcon, void *source, OSStatus status, VTEncodeInfoFlags flags, CMSampleBufferRef sample) {
    (void)source; (void)flags;
    FcEncoder *e = refcon;
    if (status != noErr) { e->callback_status = status; return; }
    if (!sample || !CMSampleBufferDataIsReady(sample)) return;
    CMBlockBufferRef block = CMSampleBufferGetDataBuffer(sample);
    size_t total = 0;
    if (!block || CMBlockBufferGetDataLength(block) == 0) return;
    total = CMBlockBufferGetDataLength(block);
    uint8_t *avcc = malloc(total);
    if (!avcc) { e->callback_status = kVTVideoEncoderMalfunctionErr; return; }
    if (CMBlockBufferCopyDataBytes(block, 0, total, avcc) != noErr) { free(avcc); e->callback_status = kVTVideoEncoderMalfunctionErr; return; }
    CMFormatDescriptionRef desc = CMSampleBufferGetFormatDescription(sample);
    bool key = true;
    CFArrayRef attachments = CMSampleBufferGetSampleAttachmentsArray(sample, false);
    if (attachments && CFArrayGetCount(attachments)) {
        CFDictionaryRef item = CFArrayGetValueAtIndex(attachments, 0);
        if (CFDictionaryContainsKey(item, kCMSampleAttachmentKey_NotSync)) key = false;
    }
    size_t prefix = 0, sps_n = 0, pps_n = 0;
    const uint8_t *sps = NULL, *pps = NULL;
    if (key && desc && CMVideoFormatDescriptionGetH264ParameterSetAtIndex(desc, 0, &sps, &sps_n, NULL, NULL) == noErr &&
        CMVideoFormatDescriptionGetH264ParameterSetAtIndex(desc, 1, &pps, &pps_n, NULL, NULL) == noErr)
        prefix = 2 * FC_START_CODE_SIZE + sps_n + pps_n;
    if (total > (SIZE_MAX - prefix - 16) / 5) { free(avcc); e->callback_status = kVTVideoEncoderMalfunctionErr; return; }
    uint8_t *annexb = malloc(prefix + 5 * total + 16);
    if (!annexb) { free(avcc); e->callback_status = kVTVideoEncoderMalfunctionErr; return; }
    size_t out = 0;
    if (prefix) {
        static const uint8_t start[] = {0,0,0,1};
        memcpy(annexb + out, start, 4); out += 4; memcpy(annexb + out, sps, sps_n); out += sps_n;
        memcpy(annexb + out, start, 4); out += 4; memcpy(annexb + out, pps, pps_n); out += pps_n;
    }
    size_t at = 0;
    while (at + 4 <= total) {
        uint32_t n = ((uint32_t)avcc[at] << 24) | ((uint32_t)avcc[at+1] << 16) | ((uint32_t)avcc[at+2] << 8) | avcc[at+3];
        at += 4;
        if (!n || n > total - at) { free(avcc); free(annexb); e->callback_status = kVTVideoEncoderMalfunctionErr; return; }
        annexb[out++] = 0; annexb[out++] = 0; annexb[out++] = 0; annexb[out++] = 1;
        memcpy(annexb + out, avcc + at, n); out += n; at += n;
    }
    CMTime pts = CMSampleBufferGetPresentationTimeStamp(sample);
    uint64_t ns = pts.timescale > 0 && pts.value >= 0 ? (uint64_t)((long double)pts.value * 1000000000.0L / pts.timescale) : 0;
    if (e->callback) e->callback(e->callback_context, annexb, out, ns, key);
    free(avcc); free(annexb);
}

void *fc_vt_encoder_create(uint32_t width, uint32_t height, uint32_t fps, uint32_t bitrate, uint32_t key_interval, int hardware, int *status, int *actual_hardware) {
    FcEncoder *e = calloc(1, sizeof(*e));
    if (!e) { *status = memFullErr; return NULL; }
    e->width = width; e->height = height;
    const void *spec_keys[] = { kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder };
    const void *spec_values[] = { hardware ? kCFBooleanTrue : kCFBooleanFalse };
    CFDictionaryRef spec = CFDictionaryCreate(kCFAllocatorDefault, spec_keys, spec_values, 1, &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks);
    CFDictionaryRef attrs = pixel_attributes(width, height);
    *status = VTCompressionSessionCreate(kCFAllocatorDefault, width, height, kCMVideoCodecType_H264, spec, attrs, kCFAllocatorDefault, compression_output, e, &e->session);
    CFRelease(spec); CFRelease(attrs);
    if (*status != noErr) { free(e); return NULL; }
    *actual_hardware = using_hardware((VTSessionRef)e->session, kVTCompressionPropertyKey_UsingHardwareAcceleratedVideoEncoder);
    if (*actual_hardware != (hardware != 0)) { *status = kVTVideoEncoderNotAvailableNowErr; VTCompressionSessionInvalidate(e->session); CFRelease(e->session); free(e); return NULL; }
    VTSessionSetProperty((VTSessionRef)e->session, kVTCompressionPropertyKey_ProfileLevel, kVTProfileLevel_H264_Baseline_AutoLevel);
    CFNumberRef b = number_i32((int32_t)bitrate), interval = number_i32((int32_t)key_interval), rate = number_i32((int32_t)fps), zero = number_i32(0);
    VTSessionSetProperty((VTSessionRef)e->session, kVTCompressionPropertyKey_AverageBitRate, b);
    VTSessionSetProperty((VTSessionRef)e->session, kVTCompressionPropertyKey_MaxKeyFrameInterval, interval);
    VTSessionSetProperty((VTSessionRef)e->session, kVTCompressionPropertyKey_ExpectedFrameRate, rate);
    VTSessionSetProperty((VTSessionRef)e->session, kVTCompressionPropertyKey_AllowFrameReordering, kCFBooleanFalse);
    VTSessionSetProperty((VTSessionRef)e->session, kVTCompressionPropertyKey_RealTime, kCFBooleanTrue);
    VTSessionSetProperty((VTSessionRef)e->session, kVTCompressionPropertyKey_MaxFrameDelayCount, zero);
    VTCompressionSessionPrepareToEncodeFrames(e->session);
    e->session_pool = VTCompressionSessionGetPixelBufferPool(e->session);
    if (e->session_pool) CFRetain(e->session_pool);
    int32_t pool_limit = FC_POOL_LIMIT;
    CFNumberRef threshold = CFNumberCreate(kCFAllocatorDefault, kCFNumberSInt32Type, &pool_limit);
    const void *pool_keys[] = { kCVPixelBufferPoolAllocationThresholdKey };
    const void *pool_values[] = { threshold };
    e->allocation_attributes = CFDictionaryCreate(kCFAllocatorDefault, pool_keys, pool_values, 1, &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks);
    CFRelease(threshold);
    if (!e->allocation_attributes) {
        *status = memFullErr;
        VTCompressionSessionInvalidate(e->session);
        CFRelease(e->session_pool);
        CFRelease(e->session);
        free(e);
        return NULL;
    }
    CFRelease(b); CFRelease(interval); CFRelease(rate); CFRelease(zero);
    *status = noErr;
    return e;
}

int fc_vt_encoder_encode(void *handle, const uint8_t *y, size_t ys, const uint8_t *uv, size_t uvs, uint64_t timestamp_ns, uint64_t duration_ns, int keyframe, FcEncoded callback, void *context) {
    FcEncoder *e = handle;
    e->callback = callback; e->callback_context = context; e->callback_status = noErr;
    CVPixelBufferRef pixel = NULL;
    CVReturn cv = CVPixelBufferPoolCreatePixelBufferWithAuxAttributes(kCFAllocatorDefault, e->session_pool, e->allocation_attributes, &pixel);
    if (cv != kCVReturnSuccess) { e->callback = NULL; e->callback_context = NULL; return cv; }
    cv = CVPixelBufferLockBaseAddress(pixel, 0);
    if (cv != kCVReturnSuccess) { CVPixelBufferRelease(pixel); e->callback = NULL; e->callback_context = NULL; return cv; }
    uint8_t *base_y = CVPixelBufferGetBaseAddressOfPlane(pixel, 0), *base_uv = CVPixelBufferGetBaseAddressOfPlane(pixel, 1);
    size_t dst_y = CVPixelBufferGetBytesPerRowOfPlane(pixel, 0), dst_uv = CVPixelBufferGetBytesPerRowOfPlane(pixel, 1);
    if (!base_y || !base_uv) { CVPixelBufferUnlockBaseAddress(pixel, 0); CVPixelBufferRelease(pixel); e->callback = NULL; e->callback_context = NULL; return kCVReturnError; }
    for (uint32_t row = 0; row < e->height; ++row) memcpy(base_y + row * dst_y, y + row * ys, e->width);
    for (uint32_t row = 0; row < (e->height + 1) / 2; ++row) memcpy(base_uv + row * dst_uv, uv + row * uvs, e->width);
    CVPixelBufferUnlockBaseAddress(pixel, 0);
    CMTime pts = CMTimeMake((int64_t)timestamp_ns, 1000000000), duration = CMTimeMake((int64_t)duration_ns, 1000000000);
    CFDictionaryRef frame_props = NULL;
    if (keyframe) { const void *keys[] = { kVTEncodeFrameOptionKey_ForceKeyFrame }; const void *values[] = { kCFBooleanTrue }; frame_props = CFDictionaryCreate(kCFAllocatorDefault, keys, values, 1, &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks); }
    VTEncodeInfoFlags flags = 0;
    OSStatus status = VTCompressionSessionEncodeFrame(e->session, pixel, pts, duration, frame_props, NULL, &flags);
    if (frame_props) CFRelease(frame_props);
    CVPixelBufferRelease(pixel);
    if (status == noErr) status = VTCompressionSessionCompleteFrames(e->session, pts);
    e->callback = NULL; e->callback_context = NULL;
    return status != noErr ? status : e->callback_status;
}
int fc_vt_encoder_flush(void *handle, FcEncoded callback, void *context) {
    FcEncoder *e = handle;
    e->callback = callback; e->callback_context = context; e->callback_status = noErr;
    OSStatus status = VTCompressionSessionCompleteFrames(e->session, kCMTimeInvalid);
    e->callback = NULL; e->callback_context = NULL;
    return status != noErr ? status : e->callback_status;
}


void fc_vt_encoder_destroy(void *handle) {
    FcEncoder *e = handle; if (!e) return;
    if (e->session) { VTCompressionSessionCompleteFrames(e->session, kCMTimeInvalid); VTCompressionSessionInvalidate(e->session); CFRelease(e->session); }
    if (e->session_pool) CFRelease(e->session_pool);
    if (e->allocation_attributes) CFRelease(e->allocation_attributes);
    free(e);
}

static void decompression_output(void *refcon, void *source, OSStatus status, VTDecodeInfoFlags flags, CVImageBufferRef image, CMTime pts, CMTime duration) {
    (void)source; (void)flags; (void)duration;
    FcDecoder *d = refcon;
    if (status != noErr) { d->callback_status = status; return; }
    if (!image || !d->callback) return;
    CVPixelBufferRef p = (CVPixelBufferRef)image;
    CVReturn lock_status = CVPixelBufferLockBaseAddress(p, kCVPixelBufferLock_ReadOnly);
    if (lock_status != kCVReturnSuccess) { d->callback_status = lock_status; return; }
    size_t coded_width = CVPixelBufferGetWidth(p), coded_height = CVPixelBufferGetHeight(p);
    const uint8_t *y = CVPixelBufferGetBaseAddressOfPlane(p, 0), *uv = CVPixelBufferGetBaseAddressOfPlane(p, 1);
    size_t ys = CVPixelBufferGetBytesPerRowOfPlane(p, 0), uvs = CVPixelBufferGetBytesPerRowOfPlane(p, 1);
    if (!y || !uv) { CVPixelBufferUnlockBaseAddress(p, kCVPixelBufferLock_ReadOnly); d->callback_status = kCVReturnError; return; }
    CGRect aperture = CMVideoFormatDescriptionGetCleanAperture(d->format, true);
    size_t x = (size_t)aperture.origin.x, top = (size_t)aperture.origin.y;
    size_t width = (size_t)aperture.size.width, height = (size_t)aperture.size.height;
    if (((x | top) & 1) || !width || !height || x + width > coded_width || top + height > coded_height) {
        CVPixelBufferUnlockBaseAddress(p, kCVPixelBufferLock_ReadOnly);
        d->callback_status = kVTVideoDecoderMalfunctionErr;
        return;
    }
    y += top * ys + x;
    uv += (top / 2) * uvs + x;
    uint64_t ns = pts.timescale > 0 && pts.value >= 0 ? (uint64_t)((long double)pts.value * 1000000000.0L / pts.timescale) : 0;
    d->callback(d->callback_context, y, ys, width, uv, uvs, width, width, height, ns);
    CVPixelBufferUnlockBaseAddress(p, kCVPixelBufferLock_ReadOnly);
}

static OSStatus create_decompression_session(FcDecoder *d, int hardware) {
    VTDecompressionOutputCallbackRecord cb = { decompression_output, d };
    int32_t pixel = kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange;
    CFNumberRef fmt = number_i32(pixel);
    CFDictionaryRef surface = CFDictionaryCreate(kCFAllocatorDefault, NULL, NULL, 0, &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks);
    const void *keys[] = { kCVPixelBufferPixelFormatTypeKey, kCVPixelBufferIOSurfacePropertiesKey };
    const void *values[] = { fmt, surface };
    CFDictionaryRef attrs = CFDictionaryCreate(kCFAllocatorDefault, keys, values, 2, &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks);
    const void *spec_keys[] = { kVTVideoDecoderSpecification_EnableHardwareAcceleratedVideoDecoder };
    const void *spec_values[] = { hardware ? kCFBooleanTrue : kCFBooleanFalse };
    CFDictionaryRef spec = CFDictionaryCreate(kCFAllocatorDefault, spec_keys, spec_values, 1, &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks);
    OSStatus status = VTDecompressionSessionCreate(kCFAllocatorDefault, d->format, spec, attrs, &cb, &d->session);
    CFRelease(fmt); CFRelease(surface); CFRelease(attrs); CFRelease(spec);
    if (status == noErr && using_hardware((VTSessionRef)d->session, kVTDecompressionPropertyKey_UsingHardwareAcceleratedVideoDecoder) != hardware) {
        VTDecompressionSessionInvalidate(d->session); CFRelease(d->session); d->session = NULL;
        status = kVTVideoDecoderNotAvailableNowErr;
    }
    return status;
}

static int make_format(FcDecoder *d, const uint8_t *sps, size_t sps_n, const uint8_t *pps, size_t pps_n) {
    const uint8_t *sets[] = { sps, pps }; size_t sizes[] = { sps_n, pps_n };
    OSStatus status = CMVideoFormatDescriptionCreateFromH264ParameterSets(kCFAllocatorDefault, 2, sets, sizes, 4, &d->format);
    if (status != noErr) return status;
    status = create_decompression_session(d, d->hardware);
    if (status != noErr && d->hardware && d->allow_software_fallback) {
        d->hardware = 0;
        status = create_decompression_session(d, 0);
    }
    return status;
}

void *fc_vt_decoder_create(int hardware, int allow_software_fallback, int *status) {
    if (hardware && !VTIsHardwareDecodeSupported(kCMVideoCodecType_H264)) { *status = kVTVideoDecoderNotAvailableNowErr; return NULL; }
    FcDecoder *d = calloc(1, sizeof(*d)); if (!d) { *status = memFullErr; return NULL; }
    d->hardware = hardware; d->allow_software_fallback = allow_software_fallback; *status = noErr; return d;
}

static size_t start_code(const uint8_t *p, size_t n, size_t from) {
    for (size_t i = from; i + 3 <= n; ++i) if (!p[i] && !p[i+1] && p[i+2] == 1) return i;
    return n;
}

int fc_vt_decoder_decode(void *handle, const uint8_t *data, size_t len, uint64_t timestamp_ns, FcDecoded callback, void *context) {
    FcDecoder *d = handle; d->callback = callback; d->callback_context = context; d->callback_status = noErr;
    size_t sps_at = 0, sps_n = 0, pps_at = 0, pps_n = 0;
    size_t at = start_code(data, len, 0);
    while (at < len) {
        size_t sc = data[at+2] == 1 ? 3 : 4;
        size_t nal = at + sc, end = start_code(data, len, nal);
        while (end > nal && data[end-1] == 0) --end;
        if (end > nal) { uint8_t type = data[nal] & 31; if (type == 7) { sps_at = nal; sps_n = end - nal; } if (type == 8) { pps_at = nal; pps_n = end - nal; } }
        at = end == len ? len : end;
        if (at < len) at = start_code(data, len, at);
    }
    if (sps_n && pps_n && (sps_n != d->sps_size || pps_n != d->pps_size || memcmp(data+sps_at,d->sps,sps_n) || memcmp(data+pps_at,d->pps,pps_n))) {
        if (d->session) { VTDecompressionSessionWaitForAsynchronousFrames(d->session); VTDecompressionSessionInvalidate(d->session); CFRelease(d->session); d->session = NULL; }
        if (d->format) { CFRelease(d->format); d->format = NULL; }
        free(d->sps); free(d->pps); d->sps = malloc(sps_n); d->pps = malloc(pps_n);
        if (!d->sps || !d->pps) return memFullErr;
        memcpy(d->sps,data+sps_at,sps_n); memcpy(d->pps,data+pps_at,pps_n); d->sps_size=sps_n; d->pps_size=pps_n;
        int status = make_format(d,d->sps,d->sps_size,d->pps,d->pps_size); if (status) return status;
    }
    if (!d->session) return kVTVideoDecoderNotAvailableNowErr;
    uint8_t *avcc = malloc(len + 4 * len / 3 + 8); if (!avcc) return memFullErr;
    size_t out = 0; at = start_code(data, len, 0);
    while (at < len) {
        size_t sc = data[at+2] == 1 ? 3 : 4; size_t nal = at + sc, end = start_code(data,len,nal);
        while (end > nal && data[end-1] == 0) --end;
        size_t n = end - nal;
        if (n) { avcc[out++] = (uint8_t)(n>>24); avcc[out++] = (uint8_t)(n>>16); avcc[out++] = (uint8_t)(n>>8); avcc[out++] = (uint8_t)n; memcpy(avcc+out,data+nal,n); out += n; }
        at = end == len ? len : start_code(data,len,end);
    }
    CMBlockBufferRef block = NULL;
    OSStatus status = CMBlockBufferCreateWithMemoryBlock(kCFAllocatorDefault, avcc, out, kCFAllocatorNull, NULL, 0, out, 0, &block);
    if (status != noErr) { free(avcc); return status; }
    CMSampleTimingInfo timing = { CMTimeMake(33333333,1000000000), CMTimeMake((int64_t)timestamp_ns,1000000000), kCMTimeInvalid };
    size_t sample_size = out;
    CMSampleBufferRef sample = NULL;
    status = CMSampleBufferCreateReady(kCFAllocatorDefault, block, d->format, 1, 1, &timing, 1, &sample_size, &sample);
    if (status == noErr) { VTDecodeInfoFlags flags = 0; status = VTDecompressionSessionDecodeFrame(d->session, sample, 0, NULL, &flags); }
    if (sample) CFRelease(sample); CFRelease(block); free(avcc);
    d->callback = NULL; d->callback_context = NULL;
    return status != noErr ? status : d->callback_status;
}

int fc_vt_decoder_is_hardware(void *handle) {
    FcDecoder *d = handle;
    if (!d || !d->session) return -1;
    return using_hardware((VTSessionRef)d->session, kVTDecompressionPropertyKey_UsingHardwareAcceleratedVideoDecoder);
}

void fc_vt_decoder_destroy(void *handle) {
    FcDecoder *d = handle; if (!d) return;
    if (d->session) { VTDecompressionSessionWaitForAsynchronousFrames(d->session); VTDecompressionSessionInvalidate(d->session); CFRelease(d->session); }
    if (d->format) CFRelease(d->format); free(d->sps); free(d->pps); free(d);
}
