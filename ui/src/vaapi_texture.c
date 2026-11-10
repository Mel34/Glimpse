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

static GdkColorState *make_hdr10_color_state(GError **error)
{
    GdkCicpParams *params = gdk_cicp_params_new();
    gdk_cicp_params_set_color_primaries(params, 9);      /* BT.2020 */
    gdk_cicp_params_set_transfer_function(params, 16);   /* SMPTE ST 2084 */
    gdk_cicp_params_set_matrix_coefficients(params, 9);  /* BT.2020 NCL */
    gdk_cicp_params_set_range(params, GDK_CICP_RANGE_NARROW);
    GdkColorState *state = gdk_cicp_params_build_color_state(params, error);
    g_object_unref(params);
    return state;
}

static int validate_p010(
    const AVDRMFrameDescriptor *descriptor,
    int width,
    int height,
    uint64_t *modifier_out)
{
    if (!descriptor || descriptor->nb_objects < 1 ||
        descriptor->nb_objects > AV_DRM_MAX_PLANES ||
        descriptor->nb_layers != 2)
        return -1;

    const AVDRMLayerDescriptor *y = &descriptor->layers[0];
    const AVDRMLayerDescriptor *uv = &descriptor->layers[1];
    if (y->format != DRM_FORMAT_R16 || uv->format != DRM_FORMAT_GR1616 ||
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
        uint64_t rows = i == 0 ? (uint64_t)height : ((uint64_t)height + 1) / 2;
        uint64_t minimum_pitch = i == 0
            ? (uint64_t)width * 2
            : (((uint64_t)width + 1) / 2) * 4;
        uint64_t end = (uint64_t)plane->offset + (uint64_t)plane->pitch * rows;
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

    if (decoded->color_primaries != AVCOL_PRI_BT2020 ||
        decoded->color_trc != AVCOL_TRC_SMPTE2084 ||
        decoded->colorspace != AVCOL_SPC_BT2020_NCL ||
        decoded->color_range != AVCOL_RANGE_MPEG) {
        set_error_message(error_message,
            "Frame is not tagged limited-range BT.2020 NCL / PQ HDR10");
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
        set_error_message(error_message, message);
        av_frame_free(&drm);
        return NULL;
    }

    const AVDRMFrameDescriptor *descriptor =
        (const AVDRMFrameDescriptor *)drm->data[0];
    uint64_t modifier = 0;
    if (validate_p010(descriptor, decoded->width, decoded->height, &modifier) < 0) {
        set_error_message(error_message,
            "VAAPI frame does not expose the validated two-layer P010 DRM layout");
        av_frame_free(&drm);
        return NULL;
    }

    GError *error = NULL;
    GdkColorState *color_state = make_hdr10_color_state(&error);
    if (!color_state) {
        set_error_message(error_message, error ? error->message : "Creating HDR10 color state failed");
        g_clear_error(&error);
        av_frame_free(&drm);
        return NULL;
    }

    GdkDmabufTextureBuilder *builder = gdk_dmabuf_texture_builder_new();
    gdk_dmabuf_texture_builder_set_display(builder, display);
    gdk_dmabuf_texture_builder_set_width(builder, (guint)decoded->width);
    gdk_dmabuf_texture_builder_set_height(builder, (guint)decoded->height);
    gdk_dmabuf_texture_builder_set_fourcc(builder, DRM_FORMAT_P010);
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

    GdkTexture *texture = gdk_dmabuf_texture_builder_build(
        builder, release_drm_frame, drm, &error);
    g_object_unref(builder);
    if (!texture) {
        set_error_message(error_message,
            error ? error->message : "GDK rejected the P010 DMA-BUF texture");
        g_clear_error(&error);
        av_frame_free(&drm);
        return NULL;
    }
    return texture;
}
