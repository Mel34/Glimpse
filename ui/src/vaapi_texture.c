#include <gtk/gtk.h>
#include <libdrm/drm_fourcc.h>

#include <libavutil/error.h>
#include <libavutil/frame.h>
#include <libavutil/hwcontext.h>
#include <libavutil/hwcontext_drm.h>
#include <libavutil/pixdesc.h>

#include <limits.h>
#include <stdint.h>
#include <stdio.h>

static void set_error_message(char **destination, const char *message)
{
    if (destination)
        *destination = g_strdup(message);
}

static void release_drm_frame(gpointer data)
{
    AVFrame *frame = data;
    av_frame_free(&frame);
}

static int map_color_primaries(enum AVColorPrimaries value, guint *mapped)
{
    switch (value) {
    case AVCOL_PRI_BT709: *mapped = 1; return 0;
    case AVCOL_PRI_BT470BG: *mapped = 5; return 0;
    case AVCOL_PRI_SMPTE170M: *mapped = 6; return 0;
    case AVCOL_PRI_SMPTE240M: *mapped = 7; return 0;
    case AVCOL_PRI_BT2020: *mapped = 9; return 0;
    case AVCOL_PRI_SMPTE432: *mapped = 12; return 0;
    default: return -1;
    }
}

static int map_transfer_function(enum AVColorTransferCharacteristic value, guint *mapped)
{
    switch (value) {
    case AVCOL_TRC_BT709: *mapped = 1; return 0;
    case AVCOL_TRC_GAMMA22: *mapped = 4; return 0;
    case AVCOL_TRC_GAMMA28: *mapped = 5; return 0;
    case AVCOL_TRC_SMPTE170M: *mapped = 6; return 0;
    case AVCOL_TRC_LINEAR: *mapped = 8; return 0;
    case AVCOL_TRC_IEC61966_2_1: *mapped = 13; return 0;
    case AVCOL_TRC_BT2020_10: *mapped = 14; return 0;
    case AVCOL_TRC_BT2020_12: *mapped = 15; return 0;
    case AVCOL_TRC_SMPTE2084: *mapped = 16; return 0;
    case AVCOL_TRC_ARIB_STD_B67: *mapped = 18; return 0;
    default: return -1;
    }
}

static int map_matrix_coefficients(enum AVColorSpace value, guint *mapped)
{
    switch (value) {
    case AVCOL_SPC_RGB: *mapped = 0; return 0;
    case AVCOL_SPC_BT709: *mapped = 1; return 0;
    case AVCOL_SPC_BT470BG: *mapped = 5; return 0;
    case AVCOL_SPC_SMPTE170M: *mapped = 6; return 0;
    case AVCOL_SPC_BT2020_NCL: *mapped = 9; return 0;
    default: return -1;
    }
}

static GdkColorState *make_frame_color_state(const AVFrame *frame, GError **error)
{
    guint primaries, transfer, matrix;
    if (map_color_primaries(frame->color_primaries, &primaries) < 0 ||
        map_transfer_function(frame->color_trc, &transfer) < 0 ||
        map_matrix_coefficients(frame->colorspace, &matrix) < 0 ||
        (frame->color_range != AVCOL_RANGE_MPEG &&
         frame->color_range != AVCOL_RANGE_JPEG)) {
        g_set_error(error, G_IO_ERROR, G_IO_ERROR_NOT_SUPPORTED,
                    "Unsupported or incomplete video color metadata "
                    "(primaries=%d, transfer=%d, matrix=%d, range=%d)",
                    frame->color_primaries, frame->color_trc,
                    frame->colorspace, frame->color_range);
        return NULL;
    }

    GdkCicpParams *params = gdk_cicp_params_new();
    gdk_cicp_params_set_color_primaries(params, primaries);
    gdk_cicp_params_set_transfer_function(params, transfer);
    gdk_cicp_params_set_matrix_coefficients(params, matrix);
    gdk_cicp_params_set_range(
        params,
        frame->color_range == AVCOL_RANGE_JPEG
            ? GDK_CICP_RANGE_FULL
            : GDK_CICP_RANGE_NARROW);
    GdkColorState *state = gdk_cicp_params_build_color_state(params, error);
    g_object_unref(params);
    return state;
}

static int validate_420_layout(
    const AVDRMFrameDescriptor *descriptor,
    int width,
    int height,
    uint32_t y_format,
    uint32_t uv_format,
    uint32_t bytes_per_sample,
    uint64_t *modifier_out)
{
    if (!descriptor || width <= 0 || height <= 0 ||
        descriptor->nb_objects < 1 ||
        descriptor->nb_objects > AV_DRM_MAX_PLANES ||
        descriptor->nb_layers != 2)
        return -1;

    const AVDRMLayerDescriptor *y = &descriptor->layers[0];
    const AVDRMLayerDescriptor *uv = &descriptor->layers[1];
    if (y->format != y_format || uv->format != uv_format ||
        y->nb_planes != 1 || uv->nb_planes != 1)
        return -1;

    const AVDRMPlaneDescriptor *planes[2] = {
        &y->planes[0], &uv->planes[0]
    };
    uint64_t modifier = UINT64_MAX;
    for (int i = 0; i < 2; i++) {
        const AVDRMPlaneDescriptor *plane = planes[i];
        if (plane->object_index < 0 ||
            plane->object_index >= descriptor->nb_objects ||
            plane->offset < 0 || plane->offset > INT_MAX ||
            plane->pitch <= 0 || plane->pitch > INT_MAX)
            return -1;

        const AVDRMObjectDescriptor *object =
            &descriptor->objects[plane->object_index];
        uint64_t rows = i == 0
            ? (uint64_t)height
            : ((uint64_t)height + 1) / 2;
        uint64_t minimum_pitch = i == 0
            ? (uint64_t)width * bytes_per_sample
            : (((uint64_t)width + 1) / 2) * 2 * bytes_per_sample;
        uint64_t end = (uint64_t)plane->offset +
            (uint64_t)plane->pitch * rows;
        if (object->fd < 0 || object->size == 0 ||
            (uint64_t)plane->offset >= object->size ||
            (uint64_t)plane->pitch < minimum_pitch ||
            end < (uint64_t)plane->offset || end > object->size)
            return -1;

        if (modifier == UINT64_MAX)
            modifier = object->format_modifier;
        else if (modifier != object->format_modifier)
            return -1;
    }
    *modifier_out = modifier;
    return 0;
}

GdkTexture *glimpse_vaapi_texture_from_frame(
    const AVFrame *decoded,
    GdkDisplay *display,
    char **error_message)
{
    if (error_message)
        *error_message = NULL;

    if (!decoded || !display || decoded->format != AV_PIX_FMT_VAAPI) {
        set_error_message(error_message, "Expected a VAAPI-decoded video frame");
        return NULL;
    }

    AVFrame *drm = av_frame_alloc();
    if (!drm) {
        set_error_message(error_message, "Allocating DRM PRIME frame failed");
        return NULL;
    }
    drm->format = AV_PIX_FMT_DRM_PRIME;
    int result = av_hwframe_map(
        drm, (AVFrame *)decoded, AV_HWFRAME_MAP_READ | AV_HWFRAME_MAP_DIRECT);
    if (result < 0) {
        char message[AV_ERROR_MAX_STRING_SIZE];
        av_strerror(result, message, sizeof(message));
        char detailed[AV_ERROR_MAX_STRING_SIZE + 64];
        snprintf(detailed, sizeof(detailed),
                 "Mapping VAAPI surface to DRM PRIME failed: %s", message);
        set_error_message(error_message, detailed);
        av_frame_free(&drm);
        return NULL;
    }

    const AVDRMFrameDescriptor *descriptor =
        (const AVDRMFrameDescriptor *)drm->data[0];
    uint32_t fourcc;
    uint32_t y_format;
    uint32_t uv_format;
    uint32_t bytes_per_sample;
    GdkColorState *color_state = NULL;

    if (descriptor && descriptor->nb_layers == 2 &&
        descriptor->layers[0].format == DRM_FORMAT_R8 &&
        descriptor->layers[1].format == DRM_FORMAT_GR88) {
        fourcc = DRM_FORMAT_NV12;
        y_format = DRM_FORMAT_R8;
        uv_format = DRM_FORMAT_GR88;
        bytes_per_sample = 1;
        GError *error = NULL;
        color_state = make_frame_color_state(decoded, &error);
        if (!color_state) {
            set_error_message(error_message,
                error ? error->message : "Could not determine NV12 color state");
            g_clear_error(&error);
            av_frame_free(&drm);
            return NULL;
        }
    } else if (descriptor && descriptor->nb_layers == 2 &&
               descriptor->layers[0].format == DRM_FORMAT_R16 &&
               descriptor->layers[1].format == DRM_FORMAT_GR1616) {
        fourcc = DRM_FORMAT_P010;
        y_format = DRM_FORMAT_R16;
        uv_format = DRM_FORMAT_GR1616;
        bytes_per_sample = 2;
        GError *error = NULL;
        color_state = make_frame_color_state(decoded, &error);
        if (!color_state) {
            set_error_message(error_message,
                error ? error->message : "Could not determine P010 color state");
            g_clear_error(&error);
            av_frame_free(&drm);
            return NULL;
        }
    } else {
        set_error_message(error_message,
            "VAAPI frame does not expose a supported two-layer NV12 or P010 DRM layout");
        av_frame_free(&drm);
        return NULL;
    }

    uint64_t modifier = 0;
    if (validate_420_layout(
            descriptor, decoded->width, decoded->height,
            y_format, uv_format, bytes_per_sample, &modifier) < 0) {
        set_error_message(error_message,
            "VAAPI frame has invalid plane metadata or an unsupported DRM layout");
        gdk_color_state_unref(color_state);
        av_frame_free(&drm);
        return NULL;
    }

    GdkDmabufTextureBuilder *builder = gdk_dmabuf_texture_builder_new();
    gdk_dmabuf_texture_builder_set_display(builder, display);
    gdk_dmabuf_texture_builder_set_width(builder, (guint)decoded->width);
    gdk_dmabuf_texture_builder_set_height(builder, (guint)decoded->height);
    gdk_dmabuf_texture_builder_set_fourcc(builder, fourcc);
    gdk_dmabuf_texture_builder_set_modifier(builder, modifier);
    gdk_dmabuf_texture_builder_set_n_planes(builder, 2);
    gdk_dmabuf_texture_builder_set_color_state(builder, color_state);
    gdk_color_state_unref(color_state);

    for (guint i = 0; i < 2; i++) {
        const AVDRMLayerDescriptor *layer = &descriptor->layers[i];
        const AVDRMPlaneDescriptor *plane = &layer->planes[0];
        const AVDRMObjectDescriptor *object =
            &descriptor->objects[plane->object_index];
        gdk_dmabuf_texture_builder_set_fd(builder, i, object->fd);
        gdk_dmabuf_texture_builder_set_offset(builder, i, (guint64)plane->offset);
        gdk_dmabuf_texture_builder_set_stride(builder, i, (guint32)plane->pitch);
    }

    GError *error = NULL;
    GdkTexture *texture = gdk_dmabuf_texture_builder_build(
        builder, release_drm_frame, drm, &error);
    g_object_unref(builder);
    if (!texture) {
        char detailed[512];
        snprintf(detailed, sizeof(detailed), "GDK DMA-BUF texture import failed: %s",
                 error ? error->message : "unknown GDK error");
        set_error_message(error_message, detailed);
        g_clear_error(&error);
        av_frame_free(&drm);
        return NULL;
    }
    return texture;
}
