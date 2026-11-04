#include <inttypes.h>
#include <limits.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include <EGL/egl.h>
#include <EGL/eglext.h>
#define GL_GLEXT_PROTOTYPES 1
#include <GL/gl.h>
#include <GL/glext.h>
#include <libdrm/drm_fourcc.h>

#include <libavcodec/avcodec.h>
#include <libavformat/avformat.h>
#include <libavutil/error.h>
#include <libavutil/hwcontext.h>
#include <libavutil/hwcontext_drm.h>
#include <libavutil/pixdesc.h>

static enum AVPixelFormat hw_format = AV_PIX_FMT_NONE;

static void fail(const char *what, int err)
{
    char msg[AV_ERROR_MAX_STRING_SIZE];
    av_strerror(err, msg, sizeof(msg));
    fprintf(stderr, "%s: %s\n", what, msg);
    exit(1);
}

static enum AVPixelFormat get_hw_format(
    AVCodecContext *ctx,
    const enum AVPixelFormat *formats)
{
    (void)ctx;

    for (const enum AVPixelFormat *p = formats;
         *p != AV_PIX_FMT_NONE; p++) {
        if (*p == hw_format)
            return *p;
    }

    fprintf(stderr, "VAAPI pixel format not offered by decoder\n");
    return AV_PIX_FMT_NONE;
}


static int import_layer(
    EGLDisplay display,
    PFNEGLCREATEIMAGEKHRPROC create_image,
    PFNEGLDESTROYIMAGEKHRPROC destroy_image,
    PFNGLEGLIMAGETARGETTEXTURE2DOESPROC image_to_texture,
    const AVDRMFrameDescriptor *desc,
    const AVDRMLayerDescriptor *layer,
    int width,
    int height,
    int layer_index,
    EGLImageKHR *image_out,
    GLuint *texture_out)
{
    if (layer->nb_planes != 1) {
        fprintf(stderr, "Layer %d: expected one plane, got %d\n",
                layer_index, layer->nb_planes);
        return -1;
    }

    const AVDRMPlaneDescriptor *plane = &layer->planes[0];

    if (plane->object_index < 0 ||
        plane->object_index >= desc->nb_objects ||
        plane->offset < 0 || plane->offset > INT_MAX ||
        plane->pitch <= 0 || plane->pitch > INT_MAX) {
        fprintf(stderr, "Layer %d: invalid plane metadata\n", layer_index);
        return -1;
    }

    const AVDRMObjectDescriptor *object =
        &desc->objects[plane->object_index];

    uint64_t modifier = object->format_modifier;

    EGLint attrs[] = {
        EGL_WIDTH, width,
        EGL_HEIGHT, height,
        EGL_LINUX_DRM_FOURCC_EXT, (EGLint)layer->format,
        EGL_DMA_BUF_PLANE0_FD_EXT, object->fd,
        EGL_DMA_BUF_PLANE0_OFFSET_EXT, (EGLint)plane->offset,
        EGL_DMA_BUF_PLANE0_PITCH_EXT, (EGLint)plane->pitch,
        EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT, (EGLint)(uint32_t)modifier,
        EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT, (EGLint)(uint32_t)(modifier >> 32),
        EGL_NONE
    };

    EGLImageKHR image = create_image(
        display, EGL_NO_CONTEXT, EGL_LINUX_DMA_BUF_EXT,
        (EGLClientBuffer)NULL, attrs);

    if (image == EGL_NO_IMAGE_KHR) {
        fprintf(stderr, "Layer %d: eglCreateImageKHR failed (0x%x)\n",
                layer_index, eglGetError());
        return -1;
    }

    GLuint texture = 0;
    glGenTextures(1, &texture);
    glBindTexture(GL_TEXTURE_2D, texture);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_LINEAR);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_LINEAR);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE);

    image_to_texture(GL_TEXTURE_2D, (GLeglImageOES)image);

    GLenum gl_error = glGetError();
    if (gl_error != GL_NO_ERROR) {
        fprintf(stderr, "Layer %d: GL texture import failed (0x%x)\n",
                layer_index, gl_error);
        glDeleteTextures(1, &texture);
        destroy_image(display, image);
        return -1;
    }

    GLint imported_width = 0;
    GLint imported_height = 0;
    glGetTexLevelParameteriv(
        GL_TEXTURE_2D, 0, GL_TEXTURE_WIDTH, &imported_width);
    glGetTexLevelParameteriv(
        GL_TEXTURE_2D, 0, GL_TEXTURE_HEIGHT, &imported_height);

    if (imported_width != width || imported_height != height) {
        fprintf(stderr,
                "Layer %d: imported dimensions %dx%d, expected %dx%d\n",
                layer_index, imported_width, imported_height, width, height);
        glDeleteTextures(1, &texture);
        destroy_image(display, image);
        return -1;
    }

    printf("Layer %d EGL import: %dx%d, texture=%u, modifier=0x%016" PRIx64 "\n",
           layer_index, imported_width, imported_height, texture, modifier);

    *image_out = image;
    *texture_out = texture;
    return 0;
}


static GLuint compile_shader(GLenum type, const char *source)
{
    GLuint shader = glCreateShader(type);
    glShaderSource(shader, 1, &source, NULL);
    glCompileShader(shader);

    GLint ok = GL_FALSE;
    glGetShaderiv(shader, GL_COMPILE_STATUS, &ok);

    if (!ok) {
        char log[4096];
        GLsizei length = 0;
        glGetShaderInfoLog(shader, sizeof(log), &length, log);
        fprintf(stderr, "Shader compilation failed:\n%.*s\n",
                (int)length, log);
        glDeleteShader(shader);
        return 0;
    }

    return shader;
}

static int render_nv12(GLuint y_texture, GLuint uv_texture,
                       int width, int height, const char *dump_path)
{
    static const char *vertex_source =
        "#version 330 core\n"
        "out vec2 texcoord;\n"
        "void main() {\n"
        "    vec2 p = vec2((gl_VertexID << 1) & 2, gl_VertexID & 2);\n"
        "    texcoord = p;\n"
        "    gl_Position = vec4(p * 2.0 - 1.0, 0.0, 1.0);\n"
        "}\n";

    static const char *fragment_source =
        "#version 330 core\n"
        "in vec2 texcoord;\n"
        "uniform sampler2D y_texture;\n"
        "uniform sampler2D uv_texture;\n"
        "out vec4 output_color;\n"
        "void main() {\n"
        "    float y = (texture(y_texture, texcoord).r - 0.0627451)"
        " * 1.164383;\n"
        "    vec2 uv = (texture(uv_texture, texcoord).rg"
        " - vec2(0.5019608)) * 1.138393;\n"
        "    float cb = uv.r;\n"
        "    float cr = uv.g;\n"
        "    vec3 rgb = vec3(\n"
        "        y + 1.5748 * cr,\n"
        "        y - 0.1873 * cb - 0.4681 * cr,\n"
        "        y + 1.8556 * cb\n"
        "    );\n"
        "    output_color = vec4(clamp(rgb, 0.0, 1.0), 1.0);\n"
        "}\n";

    GLuint vertex = compile_shader(GL_VERTEX_SHADER, vertex_source);
    GLuint fragment = compile_shader(GL_FRAGMENT_SHADER, fragment_source);

    if (!vertex || !fragment) {
        if (vertex)
            glDeleteShader(vertex);
        if (fragment)
            glDeleteShader(fragment);
        return -1;
    }

    GLuint program = glCreateProgram();
    glAttachShader(program, vertex);
    glAttachShader(program, fragment);
    glLinkProgram(program);

    glDeleteShader(vertex);
    glDeleteShader(fragment);

    GLint linked = GL_FALSE;
    glGetProgramiv(program, GL_LINK_STATUS, &linked);

    if (!linked) {
        char log[4096];
        GLsizei length = 0;
        glGetProgramInfoLog(program, sizeof(log), &length, log);
        fprintf(stderr, "Shader linking failed:\n%.*s\n",
                (int)length, log);
        glDeleteProgram(program);
        return -1;
    }

    GLuint output_texture = 0;
    GLuint framebuffer = 0;
    GLuint vao = 0;

    glGenTextures(1, &output_texture);
    glBindTexture(GL_TEXTURE_2D, output_texture);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, width, height,
                 0, GL_RGBA, GL_UNSIGNED_BYTE, NULL);

    glGenFramebuffers(1, &framebuffer);
    glBindFramebuffer(GL_FRAMEBUFFER, framebuffer);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0,
                           GL_TEXTURE_2D, output_texture, 0);

    GLenum status = glCheckFramebufferStatus(GL_FRAMEBUFFER);
    if (status != GL_FRAMEBUFFER_COMPLETE) {
        fprintf(stderr, "Framebuffer incomplete: 0x%x\n", status);
        glDeleteFramebuffers(1, &framebuffer);
        glDeleteTextures(1, &output_texture);
        glDeleteProgram(program);
        return -1;
    }

    glGenVertexArrays(1, &vao);
    glBindVertexArray(vao);

    glViewport(0, 0, width, height);
    glDisable(GL_BLEND);
    glDisable(GL_DEPTH_TEST);

    glUseProgram(program);

    glActiveTexture(GL_TEXTURE0);
    glBindTexture(GL_TEXTURE_2D, y_texture);
    glUniform1i(glGetUniformLocation(program, "y_texture"), 0);

    glActiveTexture(GL_TEXTURE1);
    glBindTexture(GL_TEXTURE_2D, uv_texture);
    glUniform1i(glGetUniformLocation(program, "uv_texture"), 1);

    glDrawArrays(GL_TRIANGLES, 0, 3);
    glFinish();

    GLenum gl_error = glGetError();
    if (gl_error != GL_NO_ERROR) {
        fprintf(stderr, "GPU rendering failed: GL error 0x%x\n", gl_error);
        glDeleteVertexArrays(1, &vao);
        glDeleteFramebuffers(1, &framebuffer);
        glDeleteTextures(1, &output_texture);
        glDeleteProgram(program);
        return -1;
    }

    size_t pixel_count = (size_t)width * (size_t)height;
    unsigned char *pixels = malloc(pixel_count * 4);

    if (!pixels) {
        fprintf(stderr, "Could not allocate framebuffer readback\n");
        glDeleteVertexArrays(1, &vao);
        glDeleteFramebuffers(1, &framebuffer);
        glDeleteTextures(1, &output_texture);
        glDeleteProgram(program);
        return -1;
    }

    glPixelStorei(GL_PACK_ALIGNMENT, 1);
    glReadPixels(0, 0, width, height, GL_RGBA, GL_UNSIGNED_BYTE, pixels);

    unsigned int minimum[3] = { 255, 255, 255 };
    unsigned int maximum[3] = { 0, 0, 0 };
    unsigned long long sums[3] = { 0, 0, 0 };
    size_t nonblack = 0;

    for (size_t i = 0; i < pixel_count; i++) {
        for (int c = 0; c < 3; c++) {
            unsigned int value = pixels[i * 4 + c];

            if (value < minimum[c])
                minimum[c] = value;
            if (value > maximum[c])
                maximum[c] = value;

            sums[c] += value;
        }

        if (pixels[i * 4] > 2 ||
            pixels[i * 4 + 1] > 2 ||
            pixels[i * 4 + 2] > 2)
            nonblack++;
    }

    printf("\nGPU shader rendering result\n");
    printf("Output: %dx%d RGBA8 framebuffer\n", width, height);

    for (int c = 0; c < 3; c++) {
        const char *name = c == 0 ? "R" : c == 1 ? "G" : "B";
        printf("%s: min=%u max=%u mean=%.2f\n",
               name, minimum[c], maximum[c],
               (double)sums[c] / (double)pixel_count);
    }

    printf("Non-black pixels: %zu / %zu (%.2f%%)\n",
           nonblack, pixel_count,
           100.0 * (double)nonblack / (double)pixel_count);

    printf("Center pixel: R=%u G=%u B=%u\n",
           pixels[((size_t)(height / 2) * width + width / 2) * 4],
           pixels[((size_t)(height / 2) * width + width / 2) * 4 + 1],
           pixels[((size_t)(height / 2) * width + width / 2) * 4 + 2]);
    FILE *dump = fopen(dump_path, "wb");
    int dump_ok = 1;

    if (!dump) {
        perror("Opening GPU framebuffer dump");
        dump_ok = 0;
    } else {
        size_t written = fwrite(pixels, 4, pixel_count, dump);
        int close_result = fclose(dump);

        if (written != pixel_count || close_result != 0) {
            fprintf(stderr, "Writing GPU framebuffer dump failed\n");
            dump_ok = 0;
        } else {
            printf("GPU framebuffer saved: %s\n", dump_path);
        }
    }

    int valid = dump_ok && nonblack > 0 &&
                maximum[0] > minimum[0] &&
                maximum[1] > minimum[1] &&
                maximum[2] > minimum[2];

    free(pixels);
    glBindVertexArray(0);
    glUseProgram(0);
    glDeleteVertexArrays(1, &vao);
    glDeleteFramebuffers(1, &framebuffer);
    glDeleteTextures(1, &output_texture);
    glDeleteProgram(program);

    if (!valid) {
        fprintf(stderr, "Rendered output failed basic pixel checks\n");
        return -1;
    }

    return 0;
}

static int test_egl_import(const AVDRMFrameDescriptor *desc,
                           int frame_width, int frame_height,
                           const char *dump_path)
{
    EGLDisplay display = eglGetPlatformDisplay(
        EGL_PLATFORM_SURFACELESS_MESA, EGL_DEFAULT_DISPLAY, NULL);

    if (display == EGL_NO_DISPLAY) {
        fprintf(stderr, "Could not get surfaceless EGL display: 0x%x\n",
                eglGetError());
        return -1;
    }

    EGLint major = 0, minor = 0;
    if (!eglInitialize(display, &major, &minor)) {
        fprintf(stderr, "eglInitialize failed: 0x%x\n", eglGetError());
        return -1;
    }

    printf("\nEGL version: %d.%d\n", major, minor);

    const char *extensions = eglQueryString(display, EGL_EXTENSIONS);
    if (!extensions ||
        !strstr(extensions, "EGL_EXT_image_dma_buf_import") ||
        !strstr(extensions, "EGL_EXT_image_dma_buf_import_modifiers")) {
        fprintf(stderr, "Required EGL DMA-BUF import extensions unavailable\n");
        eglTerminate(display);
        return -1;
    }

    PFNEGLCREATEIMAGEKHRPROC create_image =
        (PFNEGLCREATEIMAGEKHRPROC)eglGetProcAddress("eglCreateImageKHR");
    PFNEGLDESTROYIMAGEKHRPROC destroy_image =
        (PFNEGLDESTROYIMAGEKHRPROC)eglGetProcAddress("eglDestroyImageKHR");
    PFNGLEGLIMAGETARGETTEXTURE2DOESPROC image_to_texture =
        (PFNGLEGLIMAGETARGETTEXTURE2DOESPROC)
            eglGetProcAddress("glEGLImageTargetTexture2DOES");

    if (!create_image || !destroy_image || !image_to_texture) {
        fprintf(stderr, "Required EGL/GL entry points unavailable\n");
        eglTerminate(display);
        return -1;
    }

    if (!eglBindAPI(EGL_OPENGL_API)) {
        fprintf(stderr, "eglBindAPI failed: 0x%x\n", eglGetError());
        eglTerminate(display);
        return -1;
    }

    const EGLint config_attrs[] = {
        EGL_SURFACE_TYPE, EGL_PBUFFER_BIT,
        EGL_RENDERABLE_TYPE, EGL_OPENGL_BIT,
        EGL_NONE
    };

    EGLConfig config;
    EGLint config_count = 0;
    if (!eglChooseConfig(display, config_attrs, &config, 1, &config_count) ||
        config_count != 1) {
        fprintf(stderr, "No suitable EGL OpenGL config: 0x%x\n", eglGetError());
        eglTerminate(display);
        return -1;
    }

    const EGLint surface_attrs[] = {
        EGL_WIDTH, 1, EGL_HEIGHT, 1, EGL_NONE
    };
    EGLSurface surface = eglCreatePbufferSurface(
        display, config, surface_attrs);

    if (surface == EGL_NO_SURFACE) {
        fprintf(stderr, "eglCreatePbufferSurface failed: 0x%x\n", eglGetError());
        eglTerminate(display);
        return -1;
    }

    const EGLint context_attrs[] = {
        EGL_CONTEXT_MAJOR_VERSION, 3,
        EGL_CONTEXT_MINOR_VERSION, 3,
        EGL_CONTEXT_OPENGL_PROFILE_MASK,
        EGL_CONTEXT_OPENGL_CORE_PROFILE_BIT,
        EGL_NONE
    };

    EGLContext context = eglCreateContext(
        display, config, EGL_NO_CONTEXT, context_attrs);

    if (context == EGL_NO_CONTEXT) {
        fprintf(stderr, "eglCreateContext failed: 0x%x\n", eglGetError());
        eglDestroySurface(display, surface);
        eglTerminate(display);
        return -1;
    }

    if (!eglMakeCurrent(display, surface, surface, context)) {
        fprintf(stderr, "eglMakeCurrent failed: 0x%x\n", eglGetError());
        eglDestroyContext(display, context);
        eglDestroySurface(display, surface);
        eglTerminate(display);
        return -1;
    }

    const char *renderer = (const char *)glGetString(GL_RENDERER);
    printf("OpenGL renderer: %s\n", renderer ? renderer : "(unknown)");

    if (!renderer || strstr(renderer, "llvmpipe") ||
        strstr(renderer, "softpipe")) {
        fprintf(stderr, "Refusing to test with a software renderer\n");
        eglMakeCurrent(display, EGL_NO_SURFACE, EGL_NO_SURFACE, EGL_NO_CONTEXT);
        eglDestroyContext(display, context);
        eglDestroySurface(display, surface);
        eglTerminate(display);
        return -1;
    }

    if (desc->nb_objects != 1 || desc->nb_layers != 2 ||
        desc->layers[0].format != DRM_FORMAT_R8 ||
        desc->layers[1].format != DRM_FORMAT_GR88) {
        fprintf(stderr, "Unexpected DRM layout for this test\n");
        eglMakeCurrent(display, EGL_NO_SURFACE, EGL_NO_SURFACE, EGL_NO_CONTEXT);
        eglDestroyContext(display, context);
        eglDestroySurface(display, surface);
        eglTerminate(display);
        return -1;
    }

    EGLImageKHR images[2] = { EGL_NO_IMAGE_KHR, EGL_NO_IMAGE_KHR };
    GLuint textures[2] = { 0, 0 };

    int result = import_layer(
        display, create_image, destroy_image, image_to_texture,
        desc, &desc->layers[0],
        frame_width, frame_height, 0, &images[0], &textures[0]);

    if (result == 0) {
        result = import_layer(
            display, create_image, destroy_image, image_to_texture,
        desc, &desc->layers[1],
            frame_width / 2, frame_height / 2, 1,
            &images[1], &textures[1]);
    }

    if (result == 0) {
        fprintf(stderr, "\nAttempting NV12 shader rendering...\n");
        result = render_nv12(
            textures[0], textures[1], frame_width, frame_height, dump_path);
        if (result == 0)
            printf("NV12 shader rendering and framebuffer verification succeeded\n");
    }

    for (int i = 0; i < 2; i++) {
        if (textures[i])
            glDeleteTextures(1, &textures[i]);
        if (images[i] != EGL_NO_IMAGE_KHR)
            destroy_image(display, images[i]);
    }

    eglMakeCurrent(display, EGL_NO_SURFACE, EGL_NO_SURFACE, EGL_NO_CONTEXT);
    eglDestroyContext(display, context);
    eglDestroySurface(display, surface);
    eglTerminate(display);

    return result;
}

static void print_fourcc(uint32_t f, char out[5])
{
    out[0] = f & 0xff;
    out[1] = (f >> 8) & 0xff;
    out[2] = (f >> 16) & 0xff;
    out[3] = (f >> 24) & 0xff;
    out[4] = '\0';
}

int main(int argc, char **argv)
{
    if (argc != 2 && argc != 3) {
        fprintf(stderr, "Usage: %s VIDEO [GPU_RGBA_OUTPUT]\n", argv[0]);
        return 2;
    }

    const char *dump_path = argc == 3
        ? argv[2]
        : "/tmp/glimpse-stage1-gpu.rgba";

    AVFormatContext *fmt = NULL;
    int ret = avformat_open_input(&fmt, argv[1], NULL, NULL);
    if (ret < 0) fail("Opening input", ret);

    ret = avformat_find_stream_info(fmt, NULL);
    if (ret < 0) fail("Finding stream information", ret);

    int stream = av_find_best_stream(
        fmt, AVMEDIA_TYPE_VIDEO, -1, -1, NULL, 0);
    if (stream < 0) fail("Finding video stream", stream);

    const AVCodecParameters *par = fmt->streams[stream]->codecpar;
    const AVCodec *codec = avcodec_find_decoder(par->codec_id);
    if (!codec) {
        fprintf(stderr, "No decoder found\n");
        return 1;
    }

    printf("Decoder: %s\n", codec->name);

    for (int i = 0; ; i++) {
        const AVCodecHWConfig *config = avcodec_get_hw_config(codec, i);
        if (!config)
            break;

        printf("Hardware config: format=%s device=%d methods=0x%x\n",
               av_get_pix_fmt_name(config->pix_fmt),
               config->device_type,
               config->methods);

        if (config->device_type == AV_HWDEVICE_TYPE_VAAPI &&
            (config->methods & AV_CODEC_HW_CONFIG_METHOD_HW_DEVICE_CTX)) {
            hw_format = config->pix_fmt;
            break;
        }
    }

    if (hw_format == AV_PIX_FMT_NONE) {
        fprintf(stderr, "Decoder has no VAAPI device-context support\n");
        return 1;
    }

    AVBufferRef *device = NULL;
    ret = av_hwdevice_ctx_create(
        &device, AV_HWDEVICE_TYPE_VAAPI,
        "/dev/dri/renderD128", NULL, 0);
    if (ret < 0) fail("Creating VAAPI device", ret);

    AVCodecContext *dec = avcodec_alloc_context3(codec);
    if (!dec) {
        fprintf(stderr, "Allocating decoder failed\n");
        return 1;
    }

    ret = avcodec_parameters_to_context(dec, par);
    if (ret < 0) fail("Copying codec parameters", ret);

    dec->hw_device_ctx = av_buffer_ref(device);
    dec->get_format = get_hw_format;

    ret = avcodec_open2(dec, codec, NULL);
    if (ret < 0) fail("Opening decoder", ret);

    AVPacket *pkt = av_packet_alloc();
    AVFrame *frame = av_frame_alloc();
    AVFrame *drm = av_frame_alloc();

    if (!pkt || !frame || !drm) {
        fprintf(stderr, "Allocating packet/frame failed\n");
        return 1;
    }

    int got_frame = 0;

    while (!got_frame && (ret = av_read_frame(fmt, pkt)) >= 0) {
        if (pkt->stream_index == stream) {
            ret = avcodec_send_packet(dec, pkt);
            if (ret < 0 && ret != AVERROR(EAGAIN))
                fail("Sending packet", ret);

            while ((ret = avcodec_receive_frame(dec, frame)) >= 0) {
                got_frame = 1;
                break;
            }

            if (!got_frame && ret != AVERROR(EAGAIN))
                fail("Receiving frame", ret);
        }

        av_packet_unref(pkt);
    }

    if (!got_frame) {
        fprintf(stderr, "No decoded frame received\n");
        return 1;
    }

    printf("\nDecoded frame: %dx%d format=%s\n",
           frame->width, frame->height,
           av_get_pix_fmt_name(frame->format));
    printf("Frame metadata: colorspace=%d range=%d chroma_location=%d\n",
           frame->colorspace, frame->color_range,
           frame->chroma_location);

    if (frame->format != hw_format) {
        fprintf(stderr, "Unexpected decoder output format\n");
        return 1;
    }

    fprintf(stderr, "Attempting direct DRM PRIME mapping...\n");

    drm->format = AV_PIX_FMT_DRM_PRIME;

    ret = av_hwframe_map(
        drm, frame, AV_HWFRAME_MAP_READ | AV_HWFRAME_MAP_DIRECT);
    if (ret < 0) fail("Direct DRM PRIME mapping", ret);

    if (drm->format != AV_PIX_FMT_DRM_PRIME || !drm->data[0]) {
        fprintf(stderr, "Invalid DRM PRIME frame\n");
        return 1;
    }

    AVDRMFrameDescriptor *desc =
        (AVDRMFrameDescriptor *)drm->data[0];

    if (desc->nb_objects < 1 ||
        desc->nb_objects > AV_DRM_MAX_PLANES ||
        desc->nb_layers < 1 ||
        desc->nb_layers > AV_DRM_MAX_PLANES) {
        fprintf(stderr, "Invalid DRM descriptor counts\n");
        return 1;
    }

    printf("\nDRM PRIME descriptor\n");
    printf("Objects: %d\n", desc->nb_objects);
    printf("Layers:  %d\n", desc->nb_layers);

    for (int i = 0; i < desc->nb_objects; i++) {
        const AVDRMObjectDescriptor *obj = &desc->objects[i];

        printf("\nObject %d:\n", i);
        printf("  fd:        %d\n", obj->fd);
        printf("  size:      %" PRIu64 "\n", obj->size);
        printf("  modifier:  0x%016" PRIx64 "\n",
               obj->format_modifier);
    }

    for (int i = 0; i < desc->nb_layers; i++) {
        const AVDRMLayerDescriptor *layer = &desc->layers[i];
        char fourcc[5];
        print_fourcc(layer->format, fourcc);

        printf("\nLayer %d:\n", i);
        printf("  format:    %s (0x%08x)\n",
               fourcc, layer->format);
        printf("  planes:    %d\n", layer->nb_planes);

        for (int p = 0; p < layer->nb_planes; p++) {
            const AVDRMPlaneDescriptor *plane = &layer->planes[p];

            printf("  plane %d: object=%d offset=%td pitch=%td\n",
                   p, plane->object_index,
                   plane->offset, plane->pitch);
        }
    }

    fprintf(stderr, "\nAttempting EGL DMA-BUF import...\n");
    if (test_egl_import(
            desc, frame->width, frame->height, dump_path) < 0) {
        fprintf(stderr, "EGL DMA-BUF import test failed\n");
        return 1;
    }
    printf("EGL DMA-BUF import and GL texture creation succeeded\n");

    av_frame_free(&drm);
    av_frame_free(&frame);
    av_packet_free(&pkt);
    avcodec_free_context(&dec);
    av_buffer_unref(&device);
    avformat_close_input(&fmt);

    return 0;
}
