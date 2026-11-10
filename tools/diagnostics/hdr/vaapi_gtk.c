/*
 * Decode one HDR10 frame through VAAPI and present the original DRM PRIME
 * P010 planes as a GDK DMA-BUF texture. This is an isolated diagnostic:
 * it does not use or modify Glimpse's production playback pipeline.
 */

#include <errno.h>
#include <glob.h>
#include <inttypes.h>
#include <limits.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include <gtk/gtk.h>
#include <libdrm/drm_fourcc.h>

#include <libavcodec/avcodec.h>
#include <libavformat/avformat.h>
#include <libavutil/error.h>
#include <libavutil/hwcontext.h>
#include <libavutil/hwcontext_drm.h>
#include <libavutil/pixdesc.h>

typedef struct {
    const char *video_path;
    uint64_t frame_index;
} ProbeConfig;

static ProbeConfig config;
static enum AVPixelFormat vaapi_format = AV_PIX_FMT_NONE;

static void log_av_error(const char *operation, int error)
{
    char message[AV_ERROR_MAX_STRING_SIZE];
    av_strerror(error, message, sizeof(message));
    fprintf(stderr, "%s: %s\n", operation, message);
}

static enum AVPixelFormat get_hw_format(
    AVCodecContext *context,
    const enum AVPixelFormat *formats)
{
    (void)context;

    for (const enum AVPixelFormat *format = formats;
         *format != AV_PIX_FMT_NONE; format++) {
        if (*format == vaapi_format)
            return *format;
    }

    fprintf(stderr, "Decoder did not offer the VAAPI pixel format\n");
    return AV_PIX_FMT_NONE;
}

static int codec_supports_vaapi(
    const AVCodec *codec,
    enum AVPixelFormat *format_out)
{
    for (int i = 0; ; i++) {
        const AVCodecHWConfig *hardware = avcodec_get_hw_config(codec, i);
        if (!hardware)
            break;

        if (hardware->device_type == AV_HWDEVICE_TYPE_VAAPI &&
            (hardware->methods & AV_CODEC_HW_CONFIG_METHOD_HW_DEVICE_CTX)) {
            *format_out = hardware->pix_fmt;
            return 1;
        }
    }

    return 0;
}

static const AVCodec *find_vaapi_decoder(
    enum AVCodecID codec_id,
    enum AVPixelFormat *format_out)
{
    const AVCodec *codec = avcodec_find_decoder(codec_id);
    if (codec && codec_supports_vaapi(codec, format_out))
        return codec;

    void *iterator = NULL;
    while ((codec = av_codec_iterate(&iterator))) {
        if (av_codec_is_decoder(codec) && codec->id == codec_id &&
            codec_supports_vaapi(codec, format_out))
            return codec;
    }

    return NULL;
}

static int create_vaapi_device(
    AVBufferRef **device_out,
    char **render_node_out)
{
    const char *configured = getenv("GLIMPSE_DRM_RENDER_NODE");
    if (configured && configured[0] != '\0') {
        int result = av_hwdevice_ctx_create(
            device_out, AV_HWDEVICE_TYPE_VAAPI, configured, NULL, 0);
        if (result < 0) {
            log_av_error("Creating VAAPI device", result);
            return -1;
        }
        *render_node_out = strdup(configured);
        if (!*render_node_out) {
            av_buffer_unref(device_out);
            return -1;
        }
        return 0;
    }

    glob_t nodes = { 0 };
    if (glob("/dev/dri/renderD*", 0, NULL, &nodes) != 0) {
        fprintf(stderr, "No DRM render nodes found\n");
        globfree(&nodes);
        return -1;
    }

    for (size_t i = 0; i < nodes.gl_pathc; i++) {
        AVBufferRef *device = NULL;
        int result = av_hwdevice_ctx_create(
            &device, AV_HWDEVICE_TYPE_VAAPI, nodes.gl_pathv[i], NULL, 0);
        if (result < 0)
            continue;

        char *node = strdup(nodes.gl_pathv[i]);
        if (!node) {
            av_buffer_unref(&device);
            globfree(&nodes);
            return -1;
        }

        *device_out = device;
        *render_node_out = node;
        globfree(&nodes);
        return 0;
    }

    globfree(&nodes);
    fprintf(stderr,
            "No accessible VAAPI render node; set GLIMPSE_DRM_RENDER_NODE\n");
    return -1;
}

/* Drain all currently available frames. Returns 1 when target is cloned. */
static int receive_frames(
    AVCodecContext *decoder,
    AVFrame *scratch,
    uint64_t target,
    uint64_t *decoded_count,
    AVFrame **selected)
{
    int result;
    while ((result = avcodec_receive_frame(decoder, scratch)) >= 0) {
        uint64_t index = (*decoded_count)++;
        if (index == target) {
            *selected = av_frame_clone(scratch);
            av_frame_unref(scratch);
            if (!*selected) {
                fprintf(stderr, "Cloning selected VAAPI frame failed\n");
                return -1;
            }
            return 1;
        }
        av_frame_unref(scratch);
    }

    if (result == AVERROR(EAGAIN) || result == AVERROR_EOF)
        return 0;

    log_av_error("Receiving decoded frame", result);
    return -1;
}

static AVFrame *decode_vaapi_frame(const char *path, uint64_t target)
{
    AVFormatContext *input = NULL;
    AVBufferRef *device = NULL;
    AVCodecContext *decoder = NULL;
    AVPacket *packet = NULL;
    AVFrame *scratch = NULL;
    AVFrame *selected = NULL;
    char *render_node = NULL;
    int video_stream = -1;
    uint64_t decoded_count = 0;
    int failed = 1;

    int result = avformat_open_input(&input, path, NULL, NULL);
    if (result < 0) {
        log_av_error("Opening video", result);
        goto cleanup;
    }

    result = avformat_find_stream_info(input, NULL);
    if (result < 0) {
        log_av_error("Reading stream information", result);
        goto cleanup;
    }

    video_stream = av_find_best_stream(
        input, AVMEDIA_TYPE_VIDEO, -1, -1, NULL, 0);
    if (video_stream < 0) {
        log_av_error("Finding video stream", video_stream);
        goto cleanup;
    }

    const AVCodecParameters *parameters =
        input->streams[video_stream]->codecpar;
    const AVCodec *codec = find_vaapi_decoder(
        parameters->codec_id, &vaapi_format);
    if (!codec) {
        fprintf(stderr, "No VAAPI-capable decoder found for this codec\n");
        goto cleanup;
    }

    printf("VAAPI decoder: %s; hardware pixel format: %s\n",
           codec->name, av_get_pix_fmt_name(vaapi_format));

    if (create_vaapi_device(&device, &render_node) < 0)
        goto cleanup;
    printf("VAAPI render node: %s\n", render_node);

    decoder = avcodec_alloc_context3(codec);
    if (!decoder) {
        fprintf(stderr, "Allocating decoder context failed\n");
        goto cleanup;
    }

    result = avcodec_parameters_to_context(decoder, parameters);
    if (result < 0) {
        log_av_error("Copying codec parameters", result);
        goto cleanup;
    }

    decoder->hw_device_ctx = av_buffer_ref(device);
    decoder->get_format = get_hw_format;
    result = avcodec_open2(decoder, codec, NULL);
    if (result < 0) {
        log_av_error("Opening VAAPI decoder", result);
        goto cleanup;
    }

    packet = av_packet_alloc();
    scratch = av_frame_alloc();
    if (!packet || !scratch) {
        fprintf(stderr, "Allocating packet/frame failed\n");
        goto cleanup;
    }

    for (;;) {
        result = av_read_frame(input, packet);
        if (result == AVERROR_EOF)
            break;
        if (result < 0) {
            log_av_error("Reading video packet", result);
            goto cleanup;
        }

        if (packet->stream_index == video_stream) {
            result = avcodec_send_packet(decoder, packet);
            if (result == AVERROR(EAGAIN)) {
                int found = receive_frames(
                    decoder, scratch, target, &decoded_count, &selected);
                if (found != 0) {
                    av_packet_unref(packet);
                    if (found > 0) {
                        failed = 0;
                        goto cleanup;
                    }
                    goto cleanup;
                }
                result = avcodec_send_packet(decoder, packet);
            }
            if (result < 0) {
                log_av_error("Sending video packet", result);
                av_packet_unref(packet);
                goto cleanup;
            }

            int found = receive_frames(
                decoder, scratch, target, &decoded_count, &selected);
            av_packet_unref(packet);
            if (found != 0) {
                if (found > 0) {
                    failed = 0;
                    goto cleanup;
                }
                goto cleanup;
            }
        } else {
            av_packet_unref(packet);
        }
    }

    result = avcodec_send_packet(decoder, NULL);
    if (result == AVERROR(EAGAIN)) {
        int found = receive_frames(
            decoder, scratch, target, &decoded_count, &selected);
        if (found != 0) {
            if (found > 0)
                failed = 0;
            goto cleanup;
        }
        result = avcodec_send_packet(decoder, NULL);
    }
    if (result < 0 && result != AVERROR_EOF) {
        log_av_error("Flushing VAAPI decoder", result);
        goto cleanup;
    }

    for (;;) {
        int found = receive_frames(
            decoder, scratch, target, &decoded_count, &selected);
        if (found > 0) {
            failed = 0;
            break;
        }
        if (found < 0)
            break;
        /* A flushed decoder should now return EOF, not EAGAIN indefinitely. */
        result = avcodec_receive_frame(decoder, scratch);
        if (result == AVERROR_EOF)
            break;
        if (result == AVERROR(EAGAIN))
            break;
        if (result < 0) {
            log_av_error("Draining VAAPI decoder", result);
            break;
        }
        av_frame_unref(scratch);
    }

    if (failed)
        fprintf(stderr, "Frame %" PRIu64 " was not decoded\n", target);

cleanup:
    av_packet_free(&packet);
    av_frame_free(&scratch);
    avcodec_free_context(&decoder);
    av_buffer_unref(&device);
    avformat_close_input(&input);
    free(render_node);
    return failed ? NULL : selected;
}

static void release_drm_frame(gpointer data)
{
    AVFrame *frame = data;
    av_frame_free(&frame);
}

static GdkColorState *make_hdr10_yuv_color_state(GError **error)
{
    GdkCicpParams *params = gdk_cicp_params_new();
    gdk_cicp_params_set_color_primaries(params, 9);       /* BT.2020 */
    gdk_cicp_params_set_transfer_function(params, 16);    /* SMPTE ST 2084 */
    gdk_cicp_params_set_matrix_coefficients(params, 9);   /* BT.2020 NCL */
    gdk_cicp_params_set_range(params, GDK_CICP_RANGE_NARROW);
    GdkColorState *state = gdk_cicp_params_build_color_state(params, error);
    g_object_unref(params);
    return state;
}

static int validate_p010_descriptor(
    const AVDRMFrameDescriptor *descriptor,
    int width,
    int height,
    uint64_t *modifier_out)
{
    if (!descriptor || descriptor->nb_objects < 1 ||
        descriptor->nb_objects > AV_DRM_MAX_PLANES ||
        descriptor->nb_layers != 2) {
        fprintf(stderr, "Unsupported DRM PRIME descriptor shape\n");
        return -1;
    }

    const AVDRMLayerDescriptor *y_layer = &descriptor->layers[0];
    const AVDRMLayerDescriptor *uv_layer = &descriptor->layers[1];
    if (y_layer->format != DRM_FORMAT_R16 ||
        uv_layer->format != DRM_FORMAT_GR1616 ||
        y_layer->nb_planes != 1 || uv_layer->nb_planes != 1) {
        fprintf(stderr, "Expected VAAPI P010 layers R16 + GR1616\n");
        return -1;
    }

    const AVDRMPlaneDescriptor *planes[2] = {
        &y_layer->planes[0], &uv_layer->planes[0]
    };
    uint64_t modifier = UINT64_MAX;

    for (int i = 0; i < 2; i++) {
        const AVDRMPlaneDescriptor *plane = planes[i];
        if (plane->object_index < 0 ||
            plane->object_index >= descriptor->nb_objects ||
            plane->offset < 0 || plane->offset > INT_MAX ||
            plane->pitch <= 0 || plane->pitch > INT_MAX) {
            fprintf(stderr, "Invalid P010 plane %d metadata\n", i);
            return -1;
        }

        const AVDRMObjectDescriptor *object =
            &descriptor->objects[plane->object_index];
        uint64_t rows = i == 0
            ? (uint64_t)height
            : ((uint64_t)height + 1) / 2;
        uint64_t minimum_pitch = i == 0
            ? (uint64_t)width * 2
            : (((uint64_t)width + 1) / 2) * 4;
        uint64_t end = (uint64_t)plane->offset +
            (uint64_t)plane->pitch * rows;
        if (object->fd < 0 || object->size == 0 ||
            (uint64_t)plane->offset >= object->size ||
            (uint64_t)plane->pitch < minimum_pitch ||
            end < (uint64_t)plane->offset || end > object->size) {
            fprintf(stderr, "Invalid P010 DMA-BUF bounds for plane %d\n", i);
            return -1;
        }

        if (modifier == UINT64_MAX)
            modifier = object->format_modifier;
        else if (modifier != object->format_modifier) {
            fprintf(stderr, "P010 planes use different DRM modifiers\n");
            return -1;
        }
    }

    *modifier_out = modifier;
    return 0;
}

static GdkTexture *import_vaapi_p010(
    GdkDisplay *display,
    AVFrame *decoded,
    char **description_out,
    GError **error)
{
    if (decoded->format != AV_PIX_FMT_VAAPI) {
        g_set_error(error, G_FILE_ERROR, G_FILE_ERROR_INVAL,
                    "Decoder frame is %s, not VAAPI",
                    av_get_pix_fmt_name(decoded->format));
        return NULL;
    }

    if (decoded->color_primaries != AVCOL_PRI_BT2020 ||
        decoded->color_trc != AVCOL_TRC_SMPTE2084 ||
        decoded->colorspace != AVCOL_SPC_BT2020_NCL ||
        decoded->color_range != AVCOL_RANGE_MPEG) {
        g_set_error(error, G_FILE_ERROR, G_FILE_ERROR_INVAL,
                    "Expected limited-range BT.2020 NCL / PQ HDR10 metadata "
                    "(primaries=%d transfer=%d matrix=%d range=%d)",
                    decoded->color_primaries, decoded->color_trc,
                    decoded->colorspace, decoded->color_range);
        return NULL;
    }

    AVFrame *drm = av_frame_alloc();
    if (!drm) {
        g_set_error_literal(error, G_FILE_ERROR, G_FILE_ERROR_NOMEM,
                            "Allocating DRM PRIME frame failed");
        return NULL;
    }

    drm->format = AV_PIX_FMT_DRM_PRIME;
    int result = av_hwframe_map(
        drm, decoded, AV_HWFRAME_MAP_READ | AV_HWFRAME_MAP_DIRECT);
    if (result < 0) {
        char message[AV_ERROR_MAX_STRING_SIZE];
        av_strerror(result, message, sizeof(message));
        g_set_error(error, G_FILE_ERROR, G_FILE_ERROR_FAILED,
                    "Direct VAAPI to DRM PRIME mapping failed: %s", message);
        av_frame_free(&drm);
        return NULL;
    }

    const AVDRMFrameDescriptor *descriptor =
        (const AVDRMFrameDescriptor *)drm->data[0];
    uint64_t modifier;
    if (validate_p010_descriptor(
            descriptor, decoded->width, decoded->height, &modifier) < 0) {
        g_set_error_literal(error, G_FILE_ERROR, G_FILE_ERROR_INVAL,
                            "VAAPI frame does not expose the supported P010 DRM layout");
        av_frame_free(&drm);
        return NULL;
    }

    GdkColorState *color_state = make_hdr10_yuv_color_state(error);
    if (!color_state) {
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

    const AVDRMLayerDescriptor *layers[2] = {
        &descriptor->layers[0], &descriptor->layers[1]
    };
    for (guint i = 0; i < 2; i++) {
        const AVDRMPlaneDescriptor *plane = &layers[i]->planes[0];
        const AVDRMObjectDescriptor *object =
            &descriptor->objects[plane->object_index];
        gdk_dmabuf_texture_builder_set_fd(builder, i, object->fd);
        gdk_dmabuf_texture_builder_set_offset(builder, i, (guint64)plane->offset);
        gdk_dmabuf_texture_builder_set_stride(builder, i, (guint32)plane->pitch);
        printf("P010 plane %u: fd=%d offset=%td stride=%td modifier=0x%016" PRIx64 "\n",
               i, object->fd, plane->offset, plane->pitch,
               object->format_modifier);
    }

    GdkTexture *texture = gdk_dmabuf_texture_builder_build(
        builder, release_drm_frame, drm, error);
    g_object_unref(builder);

    if (!texture) {
        av_frame_free(&drm);
        return NULL;
    }

    *description_out = g_strdup_printf(
        "VAAPI frame %dx%d · DRM PRIME P010 · limited-range BT.2020 NCL / PQ · "
        "frame index %" PRIu64 "\nY stride %td · UV stride %td · modifier 0x%016" PRIx64,
        decoded->width, decoded->height, config.frame_index,
        layers[0]->planes[0].pitch, layers[1]->planes[0].pitch, modifier);
    printf("GDK imported the original VAAPI DRM PRIME frame as P010\n");
    return texture;
}

static void activate(GtkApplication *application, gpointer user_data)
{
    (void)user_data;

    GtkWidget *window = gtk_application_window_new(application);
    gtk_window_set_title(GTK_WINDOW(window), "Glimpse — VAAPI P010 / GTK HDR probe");
    gtk_window_set_default_size(GTK_WINDOW(window), 1280, 800);

    GtkWidget *box = gtk_box_new(GTK_ORIENTATION_VERTICAL, 8);
    gtk_widget_set_margin_top(box, 12);
    gtk_widget_set_margin_bottom(box, 12);
    gtk_widget_set_margin_start(box, 12);
    gtk_widget_set_margin_end(box, 12);

    GtkWidget *status = gtk_label_new("Decoding one VAAPI HDR10 frame…");
    gtk_label_set_selectable(GTK_LABEL(status), TRUE);
    gtk_box_append(GTK_BOX(box), status);

    GtkWidget *picture = gtk_picture_new();
    gtk_picture_set_content_fit(GTK_PICTURE(picture), GTK_CONTENT_FIT_CONTAIN);
    gtk_widget_set_hexpand(picture, TRUE);
    gtk_widget_set_vexpand(picture, TRUE);
    gtk_box_append(GTK_BOX(box), picture);

    gtk_window_set_child(GTK_WINDOW(window), box);
    gtk_window_present(GTK_WINDOW(window));

    AVFrame *decoded = decode_vaapi_frame(config.video_path, config.frame_index);
    if (!decoded) {
        gtk_label_set_text(GTK_LABEL(status),
                           "VAAPI decode failed. See terminal output.");
        return;
    }

    printf("Decoded frame: %dx%d format=%s primaries=%d transfer=%d matrix=%d range=%d\n",
           decoded->width, decoded->height, av_get_pix_fmt_name(decoded->format),
           decoded->color_primaries, decoded->color_trc,
           decoded->colorspace, decoded->color_range);

    char *description = NULL;
    GError *error = NULL;
    GdkTexture *texture = import_vaapi_p010(
        gtk_widget_get_display(window), decoded, &description, &error);
    av_frame_free(&decoded);

    if (!texture) {
        gtk_label_set_text(GTK_LABEL(status),
                           error ? error->message : "P010 import failed");
        if (error) {
            fprintf(stderr, "GTK P010 import failed: %s\n", error->message);
            g_error_free(error);
        }
        return;
    }

    gtk_picture_set_paintable(GTK_PICTURE(picture), GDK_PAINTABLE(texture));
    gtk_label_set_text(GTK_LABEL(status), description);
    g_free(description);
    g_object_unref(texture);
}

int main(int argc, char **argv)
{
    if (argc < 2 || argc > 3) {
        fprintf(stderr, "Usage: %s HDR10_VIDEO [FRAME_INDEX]\n", argv[0]);
        return 2;
    }

    config.video_path = argv[1];
    config.frame_index = 0;
    if (argc == 3) {
        char *end = NULL;
        errno = 0;
        unsigned long long value = strtoull(argv[2], &end, 10);
        if (errno || end == argv[2] || *end != '\0' || argv[2][0] == '-') {
            fprintf(stderr, "Invalid frame index: %s\n", argv[2]);
            return 2;
        }
        config.frame_index = (uint64_t)value;
    }

    GtkApplication *application = gtk_application_new(
        "com.github.Mel34.Glimpse.VaapiHdrProbe",
        G_APPLICATION_DEFAULT_FLAGS);
    g_signal_connect(application, "activate", G_CALLBACK(activate), NULL);
    int result = g_application_run(G_APPLICATION(application), 1, argv);
    g_object_unref(application);
    return result;
}
