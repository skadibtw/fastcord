/* Narrow, synchronous bridge to the unmodified bundled dav1d source.
 * Rust never depends on Dav1dSettings/Dav1dPicture's native layouts.
 * n_threads=max_frame_delay=1: allocator callbacks and input release happen
 * on the calling thread; no frame references escape this handle.
 * After any negative status the owner must destroy the decoder: dav1d keeps
 * what it holds of a frame it gave up on (tile data referencing the input
 * buffer, among others) until the next frame header, and dav1d_flush does not
 * release it. Only dav1d_close, in fc_av1_destroy, does.
 */
#include <errno.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

#include "dav1d/dav1d.h"

#define FC_AV1_DIMENSIONS (-20001)
#define FC_AV1_POOL_LIMIT (-20002)
#define FC_AV1_FORMAT (-20003)
#define FC_AV1_BITSTREAM (-20004)
#define FC_AV1_NO_PROGRESS (-20005)
#define FC_AV1_POOL_SLOTS 12

struct fc_av1_slot {
    void *allocation;
    uint8_t *aligned;
    size_t capacity;
    int in_use;
};

struct fc_av1_decoder {
    Dav1dContext *context;
    Dav1dData input;
    Dav1dPicture picture;
    uint8_t *encoded;
    size_t encoded_limit;
    int input_busy;
    struct fc_av1_slot slots[FC_AV1_POOL_SLOTS];
    size_t pool_bytes;
    size_t pool_limit;
    size_t pool_allocations;
    unsigned max_dimension;
    unsigned max_pixels;
    int allocator_error;
};

/* Each pointer remains valid until next/reset/destroy. Lengths cover the
 * visible last row, never uninitialized padding at the end of that row. */
struct fc_av1_picture {
    const uint8_t *planes[3];
    size_t lengths[3];
    size_t strides[2];
    unsigned width;
    unsigned height;
    unsigned bitdepth;
    unsigned layout; /* dav1d: I400=0, I420=1, I422=2, I444=3 */
    int64_t timestamp_ns;
};

static int valid_dimensions(const struct fc_av1_decoder *d, int w, int h) {
    return w > 0 && h > 0 && (unsigned)w <= d->max_dimension &&
           (unsigned)h <= d->max_dimension &&
           (uint64_t)(unsigned)w * (unsigned)h <= d->max_pixels;
}

static int allocate_picture(Dav1dPicture *p, void *cookie) {
    struct fc_av1_decoder *d = cookie;
    if (!valid_dimensions(d, p->p.w, p->p.h)) {
        d->allocator_error = FC_AV1_DIMENSIONS;
        return DAV1D_ERR(ENOMEM);
    }
    if ((p->p.bpc != 8 && p->p.bpc != 10 && p->p.bpc != 12) ||
        p->p.layout < DAV1D_PIXEL_LAYOUT_I400 ||
        p->p.layout > DAV1D_PIXEL_LAYOUT_I444) {
        d->allocator_error = FC_AV1_FORMAT;
        return DAV1D_ERR(ENOMEM);
    }
    const size_t w = ((size_t)p->p.w + 127) & ~(size_t)127;
    const size_t h = ((size_t)p->p.h + 127) & ~(size_t)127;
    const int chroma = p->p.layout != DAV1D_PIXEL_LAYOUT_I400;
    const int ss_h = p->p.layout != DAV1D_PIXEL_LAYOUT_I444;
    const int ss_v = p->p.layout == DAV1D_PIXEL_LAYOUT_I420;
    size_t y_stride = w * (p->p.bpc > 8 ? 2 : 1);
    size_t uv_stride = chroma ? y_stride >> ss_h : 0;
    /* Match upstream's cache-conflict padding, alignment and overread pad. */
    if (!(y_stride & 1023)) y_stride += DAV1D_PICTURE_ALIGNMENT;
    if (chroma && !(uv_stride & 1023)) uv_stride += DAV1D_PICTURE_ALIGNMENT;
    const size_t y_size = y_stride * h;
    const size_t uv_size = uv_stride * (h >> ss_v);
    const size_t required = y_size + 2 * uv_size +
                            2 * DAV1D_PICTURE_ALIGNMENT - 1;
    struct fc_av1_slot *slot = NULL;
    for (unsigned i = 0; i < FC_AV1_POOL_SLOTS; i++) {
        struct fc_av1_slot *candidate = &d->slots[i];
        if (!candidate->in_use && candidate->capacity >= required &&
            (!slot || candidate->capacity < slot->capacity))
            slot = candidate;
    }
    if (!slot) {
        /* Reclaim unused differently-sized buffers before exceeding the byte
         * cap. Reference pictures are never reclaimed or overwritten. */
        for (unsigned i = 0; i < FC_AV1_POOL_SLOTS; i++) {
            struct fc_av1_slot *candidate = &d->slots[i];
            if (!candidate->in_use) {
                if (!slot) slot = candidate;
                if (candidate->capacity) {
                    d->pool_bytes -= candidate->capacity;
                    free(candidate->allocation);
                    memset(candidate, 0, sizeof(*candidate));
                }
            }
        }
        if (!slot || required > d->pool_limit - d->pool_bytes) {
            d->allocator_error = FC_AV1_POOL_LIMIT;
            return DAV1D_ERR(ENOMEM);
        }
        slot->allocation = malloc(required);
        if (!slot->allocation) return DAV1D_ERR(ENOMEM);
        slot->aligned = (uint8_t *)(((uintptr_t)slot->allocation +
                                    DAV1D_PICTURE_ALIGNMENT - 1) &
                                   ~(uintptr_t)(DAV1D_PICTURE_ALIGNMENT - 1));
        slot->capacity = required;
        d->pool_bytes += required;
        d->pool_allocations++;
    }
    slot->in_use = 1;
    p->data[0] = slot->aligned;
    p->data[1] = chroma ? slot->aligned + y_size : NULL;
    p->data[2] = chroma ? slot->aligned + y_size + uv_size : NULL;
    p->stride[0] = (ptrdiff_t)y_stride;
    p->stride[1] = (ptrdiff_t)uv_stride;
    p->allocator_data = slot;
    return 0;
}

static void release_picture(Dav1dPicture *p, void *cookie) {
    (void)cookie;
    ((struct fc_av1_slot *)p->allocator_data)->in_use = 0;
}

static void release_input(const uint8_t *bytes, void *cookie) {
    (void)bytes;
    ((struct fc_av1_decoder *)cookie)->input_busy = 0;
}

static int abort_decode(struct fc_av1_decoder *d, int status) {
    if (d->allocator_error) status = d->allocator_error;
    else if (status == DAV1D_ERR(EINVAL)) status = FC_AV1_BITSTREAM;
    dav1d_data_unref(&d->input);
    dav1d_picture_unref(&d->picture);
    d->allocator_error = 0;
    return status;
}

void fc_av1_destroy(struct fc_av1_decoder *d) {
    if (!d) return;
    dav1d_data_unref(&d->input);
    dav1d_picture_unref(&d->picture);
    if (d->context) dav1d_close(&d->context);
    for (unsigned i = 0; i < FC_AV1_POOL_SLOTS; i++)
        free(d->slots[i].allocation);
    free(d->encoded);
    free(d);
}

struct fc_av1_decoder *fc_av1_create(size_t encoded_limit, size_t pool_limit,
                                    unsigned max_dimension, unsigned max_pixels,
                                    int *status) {
    *status = DAV1D_ERR(ENOMEM);
    struct fc_av1_decoder *d = calloc(1, sizeof(*d));
    if (!d) return NULL;
    d->encoded_limit = encoded_limit;
    d->pool_limit = pool_limit;
    d->max_dimension = max_dimension;
    d->max_pixels = max_pixels;
    d->encoded = malloc(encoded_limit + DAV1D_PICTURE_ALIGNMENT);
    if (!d->encoded) {
        fc_av1_destroy(d);
        return NULL;
    }
    Dav1dSettings settings;
    dav1d_default_settings(&settings);
    settings.n_threads = 1;
    settings.max_frame_delay = 1;
    settings.all_layers = 0;
    settings.frame_size_limit = max_pixels;
    settings.strict_std_compliance = 1;
    settings.logger.callback = NULL;
    settings.allocator.cookie = d;
    settings.allocator.alloc_picture_callback = allocate_picture;
    settings.allocator.release_picture_callback = release_picture;
    *status = dav1d_open(&d->context, &settings);
    if (*status) {
        fc_av1_destroy(d);
        return NULL;
    }
    return d;
}

int fc_av1_submit(struct fc_av1_decoder *d, const uint8_t *bytes, size_t len,
                  int64_t timestamp_ns, int has_sequence_header) {
    if (!len || len > d->encoded_limit || d->input.sz || d->input_busy)
        return abort_decode(d, FC_AV1_BITSTREAM);
    dav1d_picture_unref(&d->picture);
    if (has_sequence_header) {
        Dav1dSequenceHeader header;
        const int status = dav1d_parse_sequence_header(&header, bytes, len);
        if (status) return abort_decode(d, status);
        if (!valid_dimensions(d, header.max_width, header.max_height))
            return abort_decode(d, FC_AV1_DIMENSIONS);
    }
    /* The trait supplies a borrowed slice, which dav1d may retain until all
     * OBUs/tiles are consumed. One reusable owned input buffer is necessary;
     * no input allocation occurs for subsequent temporal units. */
    memcpy(d->encoded, bytes, len);
    memset(d->encoded + len, 0, DAV1D_PICTURE_ALIGNMENT);
    int status = dav1d_data_wrap(&d->input, d->encoded, len, release_input, d);
    if (status) return abort_decode(d, status);
    d->input_busy = 1;
    d->input.m.timestamp = timestamp_ns;
    return 0;
}

static int export_picture(struct fc_av1_decoder *d, struct fc_av1_picture *out) {
    const Dav1dPicture *p = &d->picture;
    if (!valid_dimensions(d, p->p.w, p->p.h))
        return abort_decode(d, FC_AV1_DIMENSIONS);
    /* Identity-matrix 4:4:4 is GBR, not YUV. Do not mislabel it as NV12. */
    if (p->seq_hdr->mtrx == DAV1D_MC_IDENTITY)
        return abort_decode(d, FC_AV1_FORMAT);
    const size_t bytes_per_sample = p->p.bpc > 8 ? 2 : 1;
    const int chroma = p->p.layout != DAV1D_PIXEL_LAYOUT_I400;
    const int ss_h = p->p.layout != DAV1D_PIXEL_LAYOUT_I444;
    const int ss_v = p->p.layout == DAV1D_PIXEL_LAYOUT_I420;
    const size_t cw = ((size_t)p->p.w + ss_h) >> ss_h;
    const size_t ch = ((size_t)p->p.h + ss_v) >> ss_v;
    out->width = (unsigned)p->p.w;
    out->height = (unsigned)p->p.h;
    out->bitdepth = (unsigned)p->p.bpc;
    out->layout = (unsigned)p->p.layout;
    out->timestamp_ns = p->m.timestamp;
    out->strides[0] = (size_t)p->stride[0];
    out->strides[1] = (size_t)p->stride[1];
    out->lengths[0] = out->strides[0] * (out->height - 1) +
                      out->width * bytes_per_sample;
    out->lengths[1] = out->lengths[2] = chroma ?
        out->strides[1] * (ch - 1) + cw * bytes_per_sample : 0;
    for (unsigned i = 0; i < 3; i++) out->planes[i] = p->data[i];
    return 1;
}

/* Returns 1 for a borrowed picture, 0 after draining, or a negative status.
 * Calling get_picture after each send is required even when send succeeds:
 * send may have consumed only a prefix of the temporal unit. */
int fc_av1_next(struct fc_av1_decoder *d, struct fc_av1_picture *out) {
    dav1d_picture_unref(&d->picture);
    for (;;) {
        const size_t before = d->input.sz;
        int status = before ? dav1d_send_data(d->context, &d->input) : 0;
        if (status && status != DAV1D_ERR(EAGAIN))
            return abort_decode(d, status);
        status = dav1d_get_picture(d->context, &d->picture);
        if (!status) return export_picture(d, out);
        if (status != DAV1D_ERR(EAGAIN)) return abort_decode(d, status);
        if (!d->input.sz) {
            /* With synchronous complete temporal units, retained tile data
             * here means a frame was incomplete. Never overwrite it. */
            if (d->input_busy) return abort_decode(d, FC_AV1_BITSTREAM);
            return 0;
        }
        if (before == d->input.sz)
            return abort_decode(d, FC_AV1_NO_PROGRESS);
    }
}

size_t fc_av1_pool_bytes(const struct fc_av1_decoder *d) {
    return d->pool_bytes;
}

size_t fc_av1_pool_allocations(const struct fc_av1_decoder *d) {
    return d->pool_allocations;
}
