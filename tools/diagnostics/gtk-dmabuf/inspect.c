#include <limits.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include <gtk/gtk.h>
#include <drm_fourcc.h>

#include <libavcodec/avcodec.h>
#include <libavformat/avformat.h>
#include <libavutil/error.h>
#include <libavutil/hwcontext.h>
#include <libavutil/pixdesc.h>
#include <libavutil/hwcontext_drm.h>

typedef struct {
    AVFrame *frame;
} TextureOwner;

static const char *video_path;
static const char *render_node = "/dev/dri/renderD128";

static void log_av_error(const char *operation, int error)
{
    char message[AV_ERROR_MAX_STRING_SIZE];
    av_strerror(error, message, sizeof(message));
    fprintf(stderr, "%s: %s\n", operation, message);
}

static enum AVPixelFormat get_hw_format(
    AVCodecContext *context, const enum AVPixelFormat *formats)
{
    (void)context;

    for (const enum AVPixelFormat *format = formats;
         *format != AV_PIX_FMT_NONE; format++) {
        if (*format == AV_PIX_FMT_VAAPI)
            return *format;
    }

    return AV_PIX_FMT_NONE;
}

static const AVCodec *find_vaapi_decoder(enum AVCodecID id)
{
    const AVCodec *codec = NULL;
    void *iterator = NULL;

    while ((codec = av_codec_iterate(&iterator))) {
        if (!av_codec_is_decoder(codec) || codec->id != id)
            continue;

        for (int i = 0; ; i++) {
            const AVCodecHWConfig *config = avcodec_get_hw_config(codec, i);
            if (!config)
                break;

            if (config->device_type == AV_HWDEVICE_TYPE_VAAPI &&
                (config->methods & AV_CODEC_HW_CONFIG_METHOD_HW_DEVICE_CTX))
                return codec;
        }
    }

    return NULL;
}

static AVFrame *decode_first_frame(const char *path, GError **error)
{
    AVFormatContext *format = NULL;
    AVCodecContext *decoder = NULL;
    AVBufferRef *device = NULL;
    AVPacket *packet = NULL;
    AVFrame *frame = NULL;
    const AVCodec *codec;
    int result;

    result = avformat_open_input(&format, path, NULL, NULL);
    if (result < 0) {
        log_av_error("Opening video", result);
        goto fail;
    }

    result = avformat_find_stream_info(format, NULL);
    if (result < 0) {
        log_av_error("Reading stream information", result);
        goto fail;
    }

    int stream_index = av_find_best_stream(
        format, AVMEDIA_TYPE_VIDEO, -1, -1, NULL, 0);
    if (stream_index < 0) {
        log_av_error("Finding video stream", stream_index);
        goto fail;
    }

    codec = find_vaapi_decoder(
        format->streams[stream_index]->codecpar->codec_id);
    if (!codec) {
        g_set_error_literal(error, G_IO_ERROR, G_IO_ERROR_NOT_SUPPORTED,
                            "No decoder with VAAPI device-context support");
        goto fail;
    }

    fprintf(stderr, "VAAPI decoder: %s\n", codec->name);

    result = av_hwdevice_ctx_create(
        &device, AV_HWDEVICE_TYPE_VAAPI, render_node, NULL, 0);
    if (result < 0) {
        log_av_error("Creating VAAPI device", result);
        goto fail;
    }

    decoder = avcodec_alloc_context3(codec);
    if (!decoder) {
        g_set_error_literal(error, G_IO_ERROR, G_IO_ERROR_NO_SPACE,
                            "Allocating decoder failed");
        goto fail;
    }

    result = avcodec_parameters_to_context(
        decoder, format->streams[stream_index]->codecpar);
    if (result < 0) {
        log_av_error("Copying codec parameters", result);
        goto fail;
    }

    decoder->hw_device_ctx = av_buffer_ref(device);
    decoder->get_format = get_hw_format;

    result = avcodec_open2(decoder, codec, NULL);
    if (result < 0) {
        log_av_error("Opening VAAPI decoder", result);
        goto fail;
    }

    packet = av_packet_alloc();
    frame = av_frame_alloc();
    if (!packet || !frame) {
        g_set_error_literal(error, G_IO_ERROR, G_IO_ERROR_NO_SPACE,
                            "Allocating packet or frame failed");
        goto fail;
    }

    while ((result = av_read_frame(format, packet)) >= 0) {
        if (packet->stream_index != stream_index) {
            av_packet_unref(packet);
            continue;
        }

        result = avcodec_send_packet(decoder, packet);
        av_packet_unref(packet);

        if (result == AVERROR(EAGAIN)) {
            result = avcodec_receive_frame(decoder, frame);
            if (result >= 0)
                goto decoded;

            log_av_error("Receiving frame after decoder EAGAIN", result);
            goto fail;
        }

        if (result < 0) {
            log_av_error("Sending video packet", result);
            goto fail;
        }

        result = avcodec_receive_frame(decoder, frame);
        if (result >= 0)
            goto decoded;

        if (result != AVERROR(EAGAIN)) {
            log_av_error("Receiving decoded frame", result);
            goto fail;
        }
    }

    if (result != AVERROR_EOF) {
        log_av_error("Reading video packets", result);
        goto fail;
    }

    result = avcodec_send_packet(decoder, NULL);
    if (result < 0 && result != AVERROR_EOF) {
        log_av_error("Flushing decoder", result);
        goto fail;
    }

    result = avcodec_receive_frame(decoder, frame);
    if (result < 0) {
        log_av_error("Receiving flushed frame", result);
        goto fail;
    }

decoded:
    if (frame->format != AV_PIX_FMT_VAAPI) {
        g_set_error(error, G_IO_ERROR, G_IO_ERROR_NOT_SUPPORTED,
                    "Decoder returned %s instead of VAAPI",
                    av_get_pix_fmt_name(frame->format));
        goto fail;
    }

    fprintf(stderr, "Decoded frame: %dx%d, format=%s\n",
            frame->width, frame->height,
            av_get_pix_fmt_name(frame->format));

    av_packet_free(&packet);
    avcodec_free_context(&decoder);
    av_buffer_unref(&device);
    avformat_close_input(&format);
    return frame;

fail:
    if (error && !*error)
        g_set_error_literal(error, G_IO_ERROR, G_IO_ERROR_FAILED,
                            "VAAPI frame decoding failed");

    av_frame_free(&frame);
    av_packet_free(&packet);
    avcodec_free_context(&decoder);
    av_buffer_unref(&device);
    avformat_close_input(&format);
    return NULL;
}

static void texture_owner_destroy(gpointer data)
{
    TextureOwner *owner = data;
    if (!owner)
        return;

    av_frame_free(&owner->frame);
    g_free(owner);
}

static GdkTexture *create_dmabuf_texture(
    GdkDisplay *display, AVFrame *vaapi_frame, GError **error)
{
    AVFrame *drm_frame = av_frame_alloc();
    if (!drm_frame) {
        g_set_error_literal(error, G_IO_ERROR, G_IO_ERROR_NO_SPACE,
                            "Allocating DRM frame failed");
        return NULL;
    }

    drm_frame->format = AV_PIX_FMT_DRM_PRIME;

    int result = av_hwframe_map(
        drm_frame, vaapi_frame, AV_HWFRAME_MAP_READ | AV_HWFRAME_MAP_DIRECT);
    if (result < 0) {
        log_av_error("Mapping VAAPI frame to DRM PRIME", result);
        g_set_error_literal(error, G_IO_ERROR, G_IO_ERROR_NOT_SUPPORTED,
                            "Direct VAAPI-to-DRM PRIME mapping failed");
        av_frame_free(&drm_frame);
        return NULL;
    }

    if (drm_frame->format != AV_PIX_FMT_DRM_PRIME || !drm_frame->data[0]) {
        g_set_error_literal(error, G_IO_ERROR, G_IO_ERROR_INVALID_DATA,
                            "Mapped frame is not a valid DRM PRIME frame");
        av_frame_free(&drm_frame);
        return NULL;
    }

    AVDRMFrameDescriptor *descriptor =
        (AVDRMFrameDescriptor *)drm_frame->data[0];

    if (descriptor->nb_objects != 1 || descriptor->nb_layers != 2) {
        g_set_error(error, G_IO_ERROR, G_IO_ERROR_NOT_SUPPORTED,
                    "Expected one DRM object and two layers; got %d objects, %d layers",
                    descriptor->nb_objects, descriptor->nb_layers);
        av_frame_free(&drm_frame);
        return NULL;
    }

    AVDRMObjectDescriptor *object = &descriptor->objects[0];
    AVDRMLayerDescriptor *y_layer = &descriptor->layers[0];
    AVDRMLayerDescriptor *uv_layer = &descriptor->layers[1];

    if (object->fd < 0 ||
        y_layer->nb_planes != 1 || uv_layer->nb_planes != 1 ||
        y_layer->format != DRM_FORMAT_R8 ||
        uv_layer->format != DRM_FORMAT_GR88 ||
        y_layer->planes[0].object_index != 0 ||
        uv_layer->planes[0].object_index != 0) {
        g_set_error_literal(error, G_IO_ERROR, G_IO_ERROR_NOT_SUPPORTED,
                            "Only the single-object R8 + GR88 NV12 layout is supported");
        av_frame_free(&drm_frame);
        return NULL;
    }

    if (y_layer->planes[0].pitch > UINT_MAX ||
        uv_layer->planes[0].pitch > UINT_MAX ||
        y_layer->planes[0].offset > UINT_MAX ||
        uv_layer->planes[0].offset > UINT_MAX) {
        g_set_error_literal(error, G_IO_ERROR, G_IO_ERROR_INVALID_DATA,
                            "DRM plane pitch or offset exceeds GDK's field range");
        av_frame_free(&drm_frame);
        return NULL;
    }

    const guint32 fourcc = DRM_FORMAT_NV12;
    const guint64 modifier = object->format_modifier;
    GdkDmabufFormats *formats = gdk_display_get_dmabuf_formats(display);

    if (formats) {
        gsize n_formats = gdk_dmabuf_formats_get_n_formats(formats);
        gboolean exact_match = FALSE;
        fprintf(stderr, "GDK advertises %zu DMA-BUF formats:\n", n_formats);

        for (gsize i = 0; i < n_formats; i++) {
            guint32 advertised_fourcc;
            guint64 advertised_modifier;

            gdk_dmabuf_formats_get_format(
                formats, i, &advertised_fourcc, &advertised_modifier);

            if (advertised_fourcc == fourcc &&
                advertised_modifier == modifier)
                exact_match = TRUE;

            if (advertised_fourcc != fourcc)
                continue;

            char fourcc_name[5] = {
                (char)(advertised_fourcc & 0xff),
                (char)((advertised_fourcc >> 8) & 0xff),
                (char)((advertised_fourcc >> 16) & 0xff),
                (char)((advertised_fourcc >> 24) & 0xff),
                '\0'
            };

            fprintf(stderr, "  %s:0x%016" G_GINT64_MODIFIER "x%s\n",
                    fourcc_name, advertised_modifier,
                    advertised_fourcc == fourcc ? "  [NV12]" : "");
        }

        fprintf(stderr,
                "NV12 exact match: %s; gdk_dmabuf_formats_contains: %s\n",
                exact_match ? "yes" : "no",
                gdk_dmabuf_formats_contains(formats, fourcc, modifier)
                    ? "yes" : "no");
    }

    if (!formats || !gdk_dmabuf_formats_contains(formats, fourcc, modifier)) {
        g_set_error(error, G_IO_ERROR, G_IO_ERROR_NOT_SUPPORTED,
                    "GDK does not advertise NV12 with modifier 0x%016"
                    G_GINT64_MODIFIER "x", modifier);
        av_frame_free(&drm_frame);
        return NULL;
    }

    TextureOwner *owner = g_new0(TextureOwner, 1);
    owner->frame = drm_frame;

    GdkDmabufTextureBuilder *builder = gdk_dmabuf_texture_builder_new();
    gdk_dmabuf_texture_builder_set_display(builder, display);
    gdk_dmabuf_texture_builder_set_width(builder, drm_frame->width);
    gdk_dmabuf_texture_builder_set_height(builder, drm_frame->height);
    gdk_dmabuf_texture_builder_set_fourcc(builder, fourcc);
    gdk_dmabuf_texture_builder_set_modifier(builder, modifier);
    gdk_dmabuf_texture_builder_set_n_planes(builder, 2);

    /* Both NV12 planes reside in the same DMA-BUF object. */
    gdk_dmabuf_texture_builder_set_fd(builder, 0, object->fd);
    gdk_dmabuf_texture_builder_set_stride(
        builder, 0, y_layer->planes[0].pitch);
    gdk_dmabuf_texture_builder_set_offset(
        builder, 0, y_layer->planes[0].offset);

    gdk_dmabuf_texture_builder_set_fd(builder, 1, object->fd);
    gdk_dmabuf_texture_builder_set_stride(
        builder, 1, uv_layer->planes[0].pitch);
    gdk_dmabuf_texture_builder_set_offset(
        builder, 1, uv_layer->planes[0].offset);

    GdkTexture *texture = gdk_dmabuf_texture_builder_build(
        builder, texture_owner_destroy, owner, error);

    g_object_unref(builder);

    if (!texture)
        texture_owner_destroy(owner);

    return texture;
}

static void activate(GtkApplication *application, gpointer user_data)
{
    (void)user_data;

    GtkWidget *window = gtk_application_window_new(application);
    gtk_window_set_title(GTK_WINDOW(window),
                         "Glimpse GTK DMA-BUF diagnostic");
    gtk_window_set_default_size(GTK_WINDOW(window), 960, 600);

    GtkWidget *box = gtk_box_new(GTK_ORIENTATION_VERTICAL, 8);
    gtk_widget_set_margin_top(box, 12);
    gtk_widget_set_margin_bottom(box, 12);
    gtk_widget_set_margin_start(box, 12);
    gtk_widget_set_margin_end(box, 12);

    GtkWidget *status = gtk_label_new("Decoding first VAAPI frame…");
    gtk_box_append(GTK_BOX(box), status);

    GtkWidget *picture = gtk_picture_new();
    gtk_picture_set_can_shrink(GTK_PICTURE(picture), TRUE);
    gtk_picture_set_content_fit(GTK_PICTURE(picture),
                                GTK_CONTENT_FIT_CONTAIN);
    gtk_widget_set_vexpand(picture, TRUE);
    gtk_box_append(GTK_BOX(box), picture);
    gtk_window_set_child(GTK_WINDOW(window), box);

    GError *error = NULL;
    AVFrame *frame = decode_first_frame(video_path, &error);

    if (frame) {
        GdkTexture *texture = create_dmabuf_texture(
            gtk_widget_get_display(window), frame, &error);
        av_frame_free(&frame);

        if (texture) {
            gtk_picture_set_paintable(
                GTK_PICTURE(picture), GDK_PAINTABLE(texture));
            gtk_label_set_text(
                GTK_LABEL(status),
                "NV12 DMA-BUF imported by GDK. "
                "Colour accuracy is not validated.");
            g_object_unref(texture);
        }
    }

    if (error) {
        gtk_label_set_text(GTK_LABEL(status), error->message);
        fprintf(stderr,
                "GTK DMA-BUF diagnostic: domain=%s code=%d message=%s\n",
                g_quark_to_string(error->domain),
                error->code,
                error->message);
        g_error_free(error);
    }

    gtk_window_present(GTK_WINDOW(window));
}

int main(int argc, char **argv)
{
    if (argc != 2) {
        fprintf(stderr, "Usage: %s VIDEO\n", argv[0]);
        return 2;
    }

    video_path = argv[1];

    const char *node = g_getenv("GLIMPSE_DRI_RENDER_NODE");
    if (node && *node)
        render_node = node;

    GtkApplication *application = gtk_application_new(
        "org.glimpse.GtkDmabufDiagnostic", G_APPLICATION_DEFAULT_FLAGS);
    g_signal_connect(application, "activate", G_CALLBACK(activate), NULL);

    char *application_argv[] = { argv[0], NULL };
    int status = g_application_run(
        G_APPLICATION(application), 1, application_argv);

    g_object_unref(application);
    return status;
}
