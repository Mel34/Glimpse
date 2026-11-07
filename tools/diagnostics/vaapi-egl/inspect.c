#include <inttypes.h>
#include <limits.h>
#include <stdint.h>
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

static void log_av_error(const char *what, int err)
{
    char msg[AV_ERROR_MAX_STRING_SIZE];
    av_strerror(err, msg, sizeof(msg));
    fprintf(stderr, "%s: %s\n", what, msg);
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

static int get_vaapi_format(
    const AVCodec *codec,
    enum AVPixelFormat *format)
{
    for (int i = 0; ; i++) {
        const AVCodecHWConfig *config = avcodec_get_hw_config(codec, i);
        if (!config)
            break;

        if (config->device_type == AV_HWDEVICE_TYPE_VAAPI &&
            (config->methods & AV_CODEC_HW_CONFIG_METHOD_HW_DEVICE_CTX)) {
            *format = config->pix_fmt;
            return 1;
        }
    }

    return 0;
}

static const AVCodec *find_vaapi_decoder(
    enum AVCodecID codec_id,
    enum AVPixelFormat *format)
{
    const AVCodec *codec = avcodec_find_decoder(codec_id);
    if (codec && get_vaapi_format(codec, format))
        return codec;

    void *opaque = NULL;
    while ((codec = av_codec_iterate(&opaque))) {
        if (!av_codec_is_decoder(codec) || codec->id != codec_id)
            continue;

        if (get_vaapi_format(codec, format))
            return codec;
    }

    return NULL;
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

static int wait_for_gpu_completion(void)
{
    GLsync fence = glFenceSync(GL_SYNC_GPU_COMMANDS_COMPLETE, 0);
    if (!fence) {
        fprintf(stderr, "Creating OpenGL completion fence failed\n");
        return -1;
    }
    glFlush();
    GLenum status;
    int waits = 0;
    do {
        status = glClientWaitSync(
            fence, GL_SYNC_FLUSH_COMMANDS_BIT, 1000000000ULL);
        waits++;
    } while (status == GL_TIMEOUT_EXPIRED && waits < 30);
    glDeleteSync(fence);
    if (status != GL_ALREADY_SIGNALED && status != GL_CONDITION_SATISFIED) {
        fprintf(stderr, "Waiting for OpenGL completion fence failed (status=0x%x)\n", status);
        return -1;
    }
    return 0;
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
    if (wait_for_gpu_completion() < 0) {
        glDeleteVertexArrays(1, &vao);
        glDeleteFramebuffers(1, &framebuffer);
        glDeleteTextures(1, &output_texture);
        glDeleteProgram(program);
        return -1;
    }

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

static int inspect_10bit_samples(GLuint y_texture, GLuint uv_texture,
                                 int width, int height,
                                 const char *dump_path)
{
    static const char *vertex_source =
        "#version 330 core\n"
        "void main() {\n"
        "    vec2 p = vec2((gl_VertexID << 1) & 2, gl_VertexID & 2);\n"
        "    gl_Position = vec4(p * 2.0 - 1.0, 0.0, 1.0);\n"
        "}\n";

    static const char *fragment_source =
        "#version 330 core\n"
        "uniform sampler2D y_texture;\n"
        "uniform sampler2D uv_texture;\n"
        "out vec4 output_samples;\n"
        "void main() {\n"
        "    ivec2 pixel = ivec2(gl_FragCoord.xy);\n"
        "    float y = texelFetch(y_texture, pixel, 0).r;\n"
        "    vec2 uv = texelFetch(uv_texture, pixel / 2, 0).rg;\n"
        "    output_samples = vec4(y, uv.r, uv.g, 1.0);\n"
        "}\n";

    if (width <= 0 || height <= 0 ||
        (width & 1) != 0 || (height & 1) != 0 ||
        !dump_path ||
        (size_t)width > SIZE_MAX / (size_t)height / (4 * sizeof(float))) {
        fprintf(stderr, "Invalid or oversized 10-bit inspection dimensions\n");
        return -1;
    }

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
        fprintf(stderr, "10-bit inspection shader linking failed:\n%.*s\n",
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
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA32F, width, height,
                 0, GL_RGBA, GL_FLOAT, NULL);

    glGenFramebuffers(1, &framebuffer);
    glBindFramebuffer(GL_FRAMEBUFFER, framebuffer);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0,
                           GL_TEXTURE_2D, output_texture, 0);

    GLenum status = glCheckFramebufferStatus(GL_FRAMEBUFFER);
    if (status != GL_FRAMEBUFFER_COMPLETE) {
        fprintf(stderr, "10-bit inspection framebuffer incomplete: 0x%x\n",
                status);
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
    if (wait_for_gpu_completion() < 0) {
        glDeleteVertexArrays(1, &vao);
        glDeleteFramebuffers(1, &framebuffer);
        glDeleteTextures(1, &output_texture);
        glDeleteProgram(program);
        return -1;
    }

    GLenum gl_error = glGetError();
    if (gl_error != GL_NO_ERROR) {
        fprintf(stderr, "10-bit inspection rendering failed: GL error 0x%x\n",
                gl_error);
        glDeleteVertexArrays(1, &vao);
        glDeleteFramebuffers(1, &framebuffer);
        glDeleteTextures(1, &output_texture);
        glDeleteProgram(program);
        return -1;
    }

    size_t pixel_count = (size_t)width * (size_t)height;
    float *pixels = malloc(pixel_count * 4 * sizeof(*pixels));

    if (!pixels) {
        fprintf(stderr, "Could not allocate 10-bit sample readback\n");
        glDeleteVertexArrays(1, &vao);
        glDeleteFramebuffers(1, &framebuffer);
        glDeleteTextures(1, &output_texture);
        glDeleteProgram(program);
        return -1;
    }

    glPixelStorei(GL_PACK_ALIGNMENT, 1);
    glReadPixels(0, 0, width, height, GL_RGBA, GL_FLOAT, pixels);

    gl_error = glGetError();
    if (gl_error != GL_NO_ERROR) {
        fprintf(stderr, "10-bit sample readback failed: GL error 0x%x\n",
                gl_error);
        free(pixels);
        glDeleteVertexArrays(1, &vao);
        glDeleteFramebuffers(1, &framebuffer);
        glDeleteTextures(1, &output_texture);
        glDeleteProgram(program);
        return -1;
    }

    const char *names[] = { "Y", "U", "V" };
    uint32_t minimum[3] = { UINT32_MAX, UINT32_MAX, UINT32_MAX };
    uint32_t maximum[3] = { 0, 0, 0 };
    size_t counts[3] = { 0, 0, 0 };
    size_t low_six_zero[3] = { 0, 0, 0 };
    size_t above_10bit[3] = { 0, 0, 0 };
    int valid = 1;

    for (int y = 0; y < height; y++) {
        for (int x = 0; x < width; x++) {
            size_t pixel_index = ((size_t)y * (size_t)width + (size_t)x) * 4;
            int sample_channels[] = { 0, 1, 2 };

            for (int c = 0; c < 3; c++) {
                if (c != 0 && ((x & 1) != 0 || (y & 1) != 0))
                    continue;

                float normalized = pixels[pixel_index + sample_channels[c]];
                if (!(normalized >= 0.0f && normalized <= 1.0f)) {
                    fprintf(stderr,
                            "Invalid normalized %s sample at (%d,%d): %f\n",
                            names[c], x, y, normalized);
                    valid = 0;
                    continue;
                }

                uint32_t value =
                    (uint32_t)(normalized * 65535.0f + 0.5f);

                if (value < minimum[c])
                    minimum[c] = value;
                if (value > maximum[c])
                    maximum[c] = value;

                counts[c]++;
                if ((value & 0x3f) == 0)
                    low_six_zero[c]++;
                if (value > 1023)
                    above_10bit[c]++;
            }
        }
    }

    printf("\n10-bit imported texture sample inspection\n");
    printf("Samples reconstructed from normalized R16 texture values\n");
    printf("Chroma samples counted once per 2x2 luma block\n");

    for (int c = 0; c < 3; c++) {
        if (counts[c] == 0) {
            fprintf(stderr, "No %s samples inspected\n", names[c]);
            valid = 0;
            continue;
        }

        printf("%s: min=%u max=%u, low 6 bits zero=%.2f%%, "
               "values >1023=%.2f%% (%zu samples)\n",
               names[c], minimum[c], maximum[c],
               100.0 * (double)low_six_zero[c] / (double)counts[c],
               100.0 * (double)above_10bit[c] / (double)counts[c],
               counts[c]);
    }

    /*
     * Export canonical yuv420p10le samples. Imported R16 values are
     * left-aligned, so shift right by six. Preserve GL readback row order;
     * the comparison tool checks both vertical orientations.
     */
    size_t raw_size = pixel_count * 3;
    uint8_t *raw = malloc(raw_size);
    if (!raw) {
        fprintf(stderr, "Could not allocate 10-bit sample dump\n");
        valid = 0;
    } else {
        size_t offset = 0;

        for (int y = 0; y < height; y++) {
            for (int x = 0; x < width; x++) {
                size_t index =
                    ((size_t)y * (size_t)width + (size_t)x) * 4;
                uint32_t value =
                    (uint32_t)(pixels[index] * 65535.0f + 0.5f);
                uint16_t sample = (uint16_t)(value >> 6);
                raw[offset++] = (uint8_t)(sample & 0xff);
                raw[offset++] = (uint8_t)(sample >> 8);
            }
        }

        for (int plane = 1; plane <= 2; plane++) {
            for (int y = 0; y < height; y += 2) {
                for (int x = 0; x < width; x += 2) {
                    size_t index =
                        ((size_t)y * (size_t)width + (size_t)x) * 4;
                    uint32_t value =
                        (uint32_t)(pixels[index + plane] * 65535.0f + 0.5f);
                    uint16_t sample = (uint16_t)(value >> 6);
                    raw[offset++] = (uint8_t)(sample & 0xff);
                    raw[offset++] = (uint8_t)(sample >> 8);
                }
            }
        }

        FILE *dump = fopen(dump_path, "wb");
        if (!dump) {
            perror("Opening 10-bit sample dump");
            valid = 0;
        } else {
            size_t written = fwrite(raw, 1, raw_size, dump);
            int close_result = fclose(dump);
            if (written != raw_size || close_result != 0) {
                fprintf(stderr, "Writing 10-bit sample dump failed\n");
                valid = 0;
            } else {
                printf("10-bit yuv420p10le samples saved: %s (%zu bytes)\n",
                       dump_path, raw_size);
            }
        }

        free(raw);
    }

    printf("Interpretation: a high proportion of values with the low "
           "six bits zero is consistent with left-aligned 10-bit samples; "
           "values at or below 1023 are consistent with right alignment.\n");
    printf("This is sample-layout inspection only, not a color-correctness "
           "or 10-bit rendering validation.\n");

    free(pixels);
    glBindVertexArray(0);
    glUseProgram(0);
    glDeleteVertexArrays(1, &vao);
    glDeleteFramebuffers(1, &framebuffer);
    glDeleteTextures(1, &output_texture);
    glDeleteProgram(program);

    return valid ? 0 : -1;
}

static void print_fourcc(uint32_t f, char out[5]);

static int is_nv12_drm_layout(const AVDRMFrameDescriptor *desc)
{
    return desc->nb_objects == 1 && desc->nb_layers == 2 &&
           desc->layers[0].format == DRM_FORMAT_R8 &&
           desc->layers[1].format == DRM_FORMAT_GR88 &&
           desc->layers[0].nb_planes == 1 &&
           desc->layers[1].nb_planes == 1;
}

static int is_10bit_drm_layout(const AVDRMFrameDescriptor *desc)
{
    return desc->nb_objects == 1 && desc->nb_layers == 2 &&
           desc->layers[0].format == DRM_FORMAT_R16 &&
           desc->layers[1].format == DRM_FORMAT_GR1616 &&
           desc->layers[0].nb_planes == 1 &&
           desc->layers[1].nb_planes == 1;
}

static int validate_drm_descriptor(const AVDRMFrameDescriptor *desc)
{
    if (desc->nb_objects < 1 ||
        desc->nb_objects > AV_DRM_MAX_PLANES ||
        desc->nb_layers < 1 ||
        desc->nb_layers > AV_DRM_MAX_PLANES) {
        fprintf(stderr, "Invalid DRM descriptor counts\n");
        return -1;
    }

    for (int i = 0; i < desc->nb_objects; i++) {
        const AVDRMObjectDescriptor *object = &desc->objects[i];

        if (object->fd < 0 || object->size == 0) {
            fprintf(stderr, "Invalid DRM object metadata in object %d\n", i);
            return -1;
        }
    }

    for (int i = 0; i < desc->nb_layers; i++) {
        const AVDRMLayerDescriptor *layer = &desc->layers[i];

        if (layer->nb_planes < 1 ||
            layer->nb_planes > AV_DRM_MAX_PLANES) {
            fprintf(stderr,
                    "Invalid plane count in DRM layer %d: %d\n",
                    i, layer->nb_planes);
            return -1;
        }

        for (int p = 0; p < layer->nb_planes; p++) {
            const AVDRMPlaneDescriptor *plane = &layer->planes[p];

            if (plane->object_index < 0 ||
                plane->object_index >= desc->nb_objects ||
                plane->offset < 0 ||
                plane->offset > INT_MAX ||
                (uint64_t)plane->offset >=
                    desc->objects[plane->object_index].size ||
                plane->pitch <= 0 ||
                plane->pitch > INT_MAX) {
                fprintf(stderr,
                        "Invalid DRM plane metadata in layer %d plane %d\n",
                        i, p);
                return -1;
            }
        }
    }

    return 0;
}

static int validate_supported_drm_layout(
    const AVDRMFrameDescriptor *desc)
{
    if (desc->nb_objects != 1 || desc->nb_layers != 2) {
        fprintf(stderr,
                "Unsupported DRM descriptor shape: objects=%d layers=%d\n",
                desc->nb_objects, desc->nb_layers);
        return 0;
    }

    if (!is_nv12_drm_layout(desc) && !is_10bit_drm_layout(desc)) {
        char first[5];
        char second[5];
        print_fourcc(desc->layers[0].format, first);
        print_fourcc(desc->layers[1].format, second);
        fprintf(stderr,
                "Unsupported DRM layer formats: %s + %s\n",
                first, second);
        return 0;
    }

    return 1;
}

static int self_test_layout_validation(void)
{
    static const struct {
        const char *name;
        uint32_t first_format;
        uint32_t second_format;
        int expected;
    } cases[] = {
        { "NV12", DRM_FORMAT_R8, DRM_FORMAT_GR88, 1 },
        { "10-bit", DRM_FORMAT_R16, DRM_FORMAT_GR1616, 1 },
        { "unsupported XRGB8888", DRM_FORMAT_XRGB8888, DRM_FORMAT_GR88, 0 },
    };

    for (size_t i = 0; i < sizeof(cases) / sizeof(cases[0]); i++) {
        AVDRMFrameDescriptor desc = { 0 };
        desc.nb_objects = 1;
        desc.nb_layers = 2;
        desc.objects[0].fd = 0;
        desc.objects[0].size = 4096;

        desc.layers[0].format = cases[i].first_format;
        desc.layers[1].format = cases[i].second_format;

        for (int layer = 0; layer < 2; layer++) {
            desc.layers[layer].nb_planes = 1;
            desc.layers[layer].planes[0].object_index = 0;
            desc.layers[layer].planes[0].offset = layer * 2048;
            desc.layers[layer].planes[0].pitch = 64;
        }

        if (validate_drm_descriptor(&desc) < 0) {
            fprintf(stderr, "FAIL: %s descriptor validation\n",
                    cases[i].name);
            return 1;
        }

        int result = validate_supported_drm_layout(&desc);
        if (result != cases[i].expected) {
            fprintf(stderr,
                    "FAIL: %s layout classification: expected %d, got %d\n",
                    cases[i].name, cases[i].expected, result);
            return 1;
        }

        printf("PASS: %s layout classification\n", cases[i].name);
    }

    AVDRMFrameDescriptor malformed = { 0 };
    malformed.nb_objects = 1;
    malformed.nb_layers = 1;
    malformed.objects[0].fd = 0;
    malformed.objects[0].size = 4096;
    malformed.layers[0].format = DRM_FORMAT_R8;
    malformed.layers[0].nb_planes = 1;
    malformed.layers[0].planes[0].object_index = 1;
    malformed.layers[0].planes[0].offset = 0;
    malformed.layers[0].planes[0].pitch = 64;

    if (validate_drm_descriptor(&malformed) == 0) {
        fprintf(stderr, "FAIL: malformed descriptor was accepted\n");
        return 1;
    }

    printf("PASS: malformed descriptor rejected\n");
    printf("All DRM layout validation self-tests passed\n");
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

    int nv12_layout = is_nv12_drm_layout(desc);
    int ten_bit_layout = is_10bit_drm_layout(desc);

    if (!nv12_layout && !ten_bit_layout) {
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

    if (result == 0 && nv12_layout) {
        fprintf(stderr, "\nAttempting NV12 shader rendering...\n");
        result = render_nv12(
            textures[0], textures[1], frame_width, frame_height, dump_path);
        if (result == 0)
            printf("NV12 shader rendering and framebuffer verification succeeded\n");
    } else if (result == 0) {
        printf("\n10-bit R16 + GR32 EGL import and GL texture "
               "creation succeeded\n");
        result = inspect_10bit_samples(
            textures[0], textures[1], frame_width, frame_height, dump_path);
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

static int query_egl_dma_buf_support(
    const AVDRMFrameDescriptor *desc)
{
    static const struct {
        uint32_t format;
        const char *name;
    } targets[] = {
        { DRM_FORMAT_R8, "R8" },
        { DRM_FORMAT_GR88, "GR88" },
        { DRM_FORMAT_R16, "R16" },
        { DRM_FORMAT_GR1616, "GR32" },
    };

    EGLDisplay display = eglGetPlatformDisplay(
        EGL_PLATFORM_SURFACELESS_MESA, EGL_DEFAULT_DISPLAY, NULL);

    if (display == EGL_NO_DISPLAY) {
        fprintf(stderr, "Could not get EGL display for format query: 0x%x\n",
                eglGetError());
        return -1;
    }

    EGLint major = 0;
    EGLint minor = 0;
    if (!eglInitialize(display, &major, &minor)) {
        fprintf(stderr, "EGL initialization for format query failed: 0x%x\n",
                eglGetError());
        return -1;
    }

    const char *extensions = eglQueryString(display, EGL_EXTENSIONS);
    if (!extensions ||
        !strstr(extensions, "EGL_EXT_image_dma_buf_import") ||
        !strstr(extensions, "EGL_EXT_image_dma_buf_import_modifiers")) {
        fprintf(stderr,
                "EGL DMA-BUF format/modifier query extensions unavailable\n");
        eglTerminate(display);
        return -1;
    }

    PFNEGLQUERYDMABUFFORMATSEXTPROC query_formats =
        (PFNEGLQUERYDMABUFFORMATSEXTPROC)
            eglGetProcAddress("eglQueryDmaBufFormatsEXT");
    PFNEGLQUERYDMABUFMODIFIERSEXTPROC query_modifiers =
        (PFNEGLQUERYDMABUFMODIFIERSEXTPROC)
            eglGetProcAddress("eglQueryDmaBufModifiersEXT");

    if (!query_formats || !query_modifiers) {
        fprintf(stderr, "EGL DMA-BUF query entry points unavailable\n");
        eglTerminate(display);
        return -1;
    }

    EGLint format_count = 0;
    if (!query_formats(display, 0, NULL, &format_count) ||
        format_count <= 0) {
        fprintf(stderr, "eglQueryDmaBufFormatsEXT failed: 0x%x\n",
                eglGetError());
        eglTerminate(display);
        return -1;
    }

    EGLint *formats = malloc((size_t)format_count * sizeof(*formats));
    if (!formats) {
        fprintf(stderr, "Allocating EGL format list failed\n");
        eglTerminate(display);
        return -1;
    }

    EGLint returned_formats = 0;
    if (!query_formats(display, format_count, formats, &returned_formats)) {
        fprintf(stderr, "Reading EGL DMA-BUF formats failed: 0x%x\n",
                eglGetError());
        free(formats);
        eglTerminate(display);
        return -1;
    }

    printf("\nEGL DMA-BUF format and modifier support\n");
    printf("Advertised DMA-BUF formats: %d\n", returned_formats);

    for (size_t i = 0; i < sizeof(targets) / sizeof(targets[0]); i++) {
        EGLint egl_format = (EGLint)targets[i].format;
        int supported = 0;

        for (EGLint j = 0; j < returned_formats; j++) {
            if ((uint32_t)formats[j] == targets[i].format) {
                supported = 1;
                break;
            }
        }

        printf("\n%s (0x%08x): %s\n",
               targets[i].name, targets[i].format,
               supported ? "advertised" : "not advertised");

        if (!supported)
            continue;

        EGLint modifier_count = 0;
        if (!query_modifiers(
                display, egl_format, 0, NULL, NULL, &modifier_count)) {
            fprintf(stderr,
                    "  Modifier query failed for %s: 0x%x\n",
                    targets[i].name, eglGetError());
            continue;
        }

        if (modifier_count <= 0) {
            printf("  No modifiers advertised\n");
            continue;
        }

        EGLuint64KHR *modifiers =
            malloc((size_t)modifier_count * sizeof(*modifiers));
        EGLBoolean *external_only =
            malloc((size_t)modifier_count * sizeof(*external_only));

        if (!modifiers || !external_only) {
            fprintf(stderr, "  Allocating modifier list failed\n");
            free(modifiers);
            free(external_only);
            free(formats);
            eglTerminate(display);
            return -1;
        }

        EGLint returned_modifiers = 0;
        if (!query_modifiers(
                display, egl_format, modifier_count,
                modifiers, external_only, &returned_modifiers)) {
            fprintf(stderr,
                    "  Reading modifiers for %s failed: 0x%x\n",
                    targets[i].name, eglGetError());
            free(modifiers);
            free(external_only);
            continue;
        }

        printf("  Advertised modifiers: %d\n", returned_modifiers);

        for (EGLint j = 0; j < returned_modifiers; j++) {
            printf("    0x%016" PRIx64 " external_only=%s\n",
                   (uint64_t)modifiers[j],
                   external_only[j] ? "yes" : "no");
        }

        for (int layer_index = 0;
             layer_index < desc->nb_layers; layer_index++) {
            const AVDRMLayerDescriptor *layer =
                &desc->layers[layer_index];

            if (layer->format != targets[i].format)
                continue;

            for (int plane_index = 0;
                 plane_index < layer->nb_planes; plane_index++) {
                const AVDRMPlaneDescriptor *plane =
                    &layer->planes[plane_index];

                if (plane->object_index < 0 ||
                    plane->object_index >= desc->nb_objects)
                    continue;

                uint64_t frame_modifier =
                    desc->objects[plane->object_index].format_modifier;
                int modifier_supported = 0;
                EGLBoolean frame_external_only = EGL_FALSE;

                for (EGLint j = 0; j < returned_modifiers; j++) {
                    if ((uint64_t)modifiers[j] == frame_modifier) {
                        modifier_supported = 1;
                        frame_external_only = external_only[j];
                        break;
                    }
                }

                printf("  Frame layer %d plane %d modifier "
                       "0x%016" PRIx64 ": %s",
                       layer_index, plane_index, frame_modifier,
                       modifier_supported ? "supported" : "NOT supported");

                if (modifier_supported)
                    printf(" (external_only=%s)",
                           frame_external_only ? "yes" : "no");

                printf("\n");
            }
        }

        free(modifiers);
        free(external_only);
    }

    free(formats);
    eglTerminate(display);
    return 0;
}

static int process_decoded_frame(AVFrame *frame,
                                  const char *dump_path,
                                  unsigned long long frame_index)
{
    AVFrame *drm = av_frame_alloc();
    int ret = 0;
    int frame_status = 1;

    if (!drm) {
        fprintf(stderr, "Allocating DRM frame failed\n");
        return -1;
    }

    printf("Selected decoded frame: %llu\n", frame_index);
    printf("\nDecoded frame: %dx%d format=%s\n",
           frame->width, frame->height,
           av_get_pix_fmt_name(frame->format));
    printf("Frame metadata: colorspace=%d range=%d chroma_location=%d\n",
           frame->colorspace, frame->color_range,
           frame->chroma_location);

    if (frame->format != hw_format) {
        fprintf(stderr, "Unexpected decoder output format\n");
        goto frame_cleanup;
    }

    fprintf(stderr, "Attempting direct DRM PRIME mapping...\n");

    drm->format = AV_PIX_FMT_DRM_PRIME;

    ret = av_hwframe_map(
        drm, frame, AV_HWFRAME_MAP_READ | AV_HWFRAME_MAP_DIRECT);
    if (ret < 0) {
        log_av_error("Direct DRM PRIME mapping", ret);
        goto frame_cleanup;
    }

    if (drm->format != AV_PIX_FMT_DRM_PRIME || !drm->data[0]) {
        fprintf(stderr, "Invalid DRM PRIME frame\n");
        goto frame_cleanup;
    }

    AVDRMFrameDescriptor *desc =
        (AVDRMFrameDescriptor *)drm->data[0];

    if (validate_drm_descriptor(desc) < 0)
        goto frame_cleanup;

    int layout_status = validate_supported_drm_layout(desc);
    if (layout_status < 0)
        goto frame_cleanup;

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

    if (layout_status == 0) {
        printf("\nRejecting unsupported DRM layout: "
               "skipping EGL capability query and import\n");
        frame_status = 2;
    } else {
        if (query_egl_dma_buf_support(desc) < 0) {
            fprintf(stderr, "EGL DMA-BUF capability query failed\n");
            goto frame_cleanup;
        }

        fprintf(stderr, "\nAttempting EGL DMA-BUF import...\n");
        if (test_egl_import(
                desc, frame->width, frame->height, dump_path) < 0) {
            fprintf(stderr, "EGL DMA-BUF import test failed\n");
            goto frame_cleanup;
        }
        printf("%s EGL DMA-BUF import and GL texture creation succeeded\n",
               is_nv12_drm_layout(desc) ? "NV12" : "10-bit R16 + GR32");
        frame_status = 0;
    }


frame_cleanup:
    av_frame_free(&drm);
    return frame_status;
}


static int receive_available_frames(
    AVCodecContext *dec,
    AVFrame *frame,
    const char *dump_path,
    unsigned long long target_frame,
    unsigned long long frame_count,
    unsigned long long *decoded_frames,
    unsigned long long *processed_frames,
    int *exit_status)
{
    int ret;

    while ((ret = avcodec_receive_frame(dec, frame)) >= 0) {
        unsigned long long frame_index = (*decoded_frames)++;

        if (frame_index >= target_frame) {
            char frame_dump_path[PATH_MAX];
            const char *output_path = dump_path;

            if (frame_count > 1) {
                int written = snprintf(
                    frame_dump_path, sizeof(frame_dump_path),
                    "%s.frame-%llu", dump_path, frame_index);
                if (written < 0 ||
                    (size_t)written >= sizeof(frame_dump_path)) {
                    fprintf(stderr, "Frame output path is too long\n");
                    av_frame_unref(frame);
                    return AVERROR(EINVAL);
                }
                output_path = frame_dump_path;
            }

            printf("\nProcessing frame %llu (%llu/%llu)\n",
                   frame_index, *processed_frames + 1, frame_count);
            int frame_result = process_decoded_frame(
                frame, output_path, frame_index);
            if (frame_result != 0) {
                if (frame_result == 2)
                    *exit_status = 2;
                av_frame_unref(frame);
                return AVERROR_EXTERNAL;
            }
            (*processed_frames)++;
        }

        av_frame_unref(frame);
        if (*processed_frames >= frame_count)
            return 0;
    }

    return ret;
}


int main(int argc, char **argv)
{
    if (argc == 2 &&
        strcmp(argv[1], "--self-test-layout-validation") == 0)
        return self_test_layout_validation();

    if (argc < 2 || argc > 5) {
        fprintf(stderr,
                "Usage: %s VIDEO [GPU_OUTPUT [FRAME_INDEX [FRAME_COUNT]]]\n"
                "       %s --self-test-layout-validation\n",
                argv[0], argv[0]);
        return 2;
    }

    const char *dump_path = argc >= 3
        ? argv[2]
        : "/tmp/glimpse-stage-gpu-output.bin";

    unsigned long long target_frame = 0;
    if (argc >= 4) {
        char *end = NULL;
        if (argv[3][0] == '-') {
            fprintf(stderr, "FRAME_INDEX must be non-negative\n");
            return 2;
        }

        target_frame = strtoull(argv[3], &end, 10);
        if (end == argv[3] || *end != '\0') {
            fprintf(stderr, "Invalid FRAME_INDEX: %s\n", argv[3]);
            return 2;
        }
    }

    unsigned long long frame_count = 1;
    if (argc == 5) {
        char *end = NULL;
        if (argv[4][0] == '-') {
            fprintf(stderr, "FRAME_COUNT must be positive\n");
            return 2;
        }
        frame_count = strtoull(argv[4], &end, 10);
        if (end == argv[4] || *end != '\0' || frame_count == 0 || frame_count > 300) {
            fprintf(stderr, "FRAME_COUNT must be between 1 and 300\n");
            return 2;
        }
    }

    AVFormatContext *fmt = NULL;
    AVBufferRef *device = NULL;
    AVCodecContext *dec = NULL;
    AVPacket *pkt = NULL;
    AVFrame *frame = NULL;
    int exit_status = 1;

    int ret = avformat_open_input(&fmt, argv[1], NULL, NULL);
    if (ret < 0) {
        log_av_error("Opening input", ret);
        goto cleanup;
    }

    ret = avformat_find_stream_info(fmt, NULL);
    if (ret < 0) {
        log_av_error("Finding stream information", ret);
        goto cleanup;
    }

    int stream = av_find_best_stream(
        fmt, AVMEDIA_TYPE_VIDEO, -1, -1, NULL, 0);
    if (stream < 0) {
        log_av_error("Finding video stream", stream);
        goto cleanup;
    }

    const AVCodecParameters *par = fmt->streams[stream]->codecpar;
    const AVCodec *codec = find_vaapi_decoder(par->codec_id, &hw_format);
    if (!codec) {
        fprintf(stderr,
                "No decoder with VAAPI device-context support found\n");
        goto cleanup;
    }

    printf("VAAPI decoder: %s\n", codec->name);

    for (int i = 0; ; i++) {
        const AVCodecHWConfig *config = avcodec_get_hw_config(codec, i);
        if (!config)
            break;

        printf("Hardware config: format=%s device=%d methods=0x%x\n",
               av_get_pix_fmt_name(config->pix_fmt),
               config->device_type,
               config->methods);

    }

    if (hw_format == AV_PIX_FMT_NONE) {
        fprintf(stderr, "Decoder has no VAAPI device-context support\n");
        goto cleanup;
    }

    ret = av_hwdevice_ctx_create(
        &device, AV_HWDEVICE_TYPE_VAAPI,
        "/dev/dri/renderD128", NULL, 0);
    if (ret < 0) {
        log_av_error("Creating VAAPI device", ret);
        goto cleanup;
    }

    dec = avcodec_alloc_context3(codec);
    if (!dec) {
        fprintf(stderr, "Allocating decoder failed\n");
        goto cleanup;
    }

    ret = avcodec_parameters_to_context(dec, par);
    if (ret < 0) {
        log_av_error("Copying codec parameters", ret);
        goto cleanup;
    }

    dec->hw_device_ctx = av_buffer_ref(device);
    dec->get_format = get_hw_format;

    ret = avcodec_open2(dec, codec, NULL);
    if (ret < 0) {
        log_av_error("Opening decoder", ret);
        goto cleanup;
    }

    pkt = av_packet_alloc();
    frame = av_frame_alloc();

    if (!pkt || !frame) {
        fprintf(stderr, "Allocating packet/frame failed\n");
        goto cleanup;
    }

    unsigned long long decoded_frames = 0;
    unsigned long long processed_frames = 0;
    int flush_requested = 0;
    int flush_sent = 0;
    int decoder_eof = 0;
    int packet_pending = 0;

    while (processed_frames < frame_count && !decoder_eof) {
        if (!packet_pending && !flush_requested && !flush_sent) {
            ret = av_read_frame(fmt, pkt);
            if (ret == AVERROR_EOF) {
                flush_requested = 1;
            } else if (ret < 0) {
                log_av_error("Reading input packet", ret);
                goto cleanup;
            } else if (pkt->stream_index == stream) {
                packet_pending = 1;
            } else {
                av_packet_unref(pkt);
            }
        }

        if (packet_pending || flush_requested) {
            ret = avcodec_send_packet(dec, packet_pending ? pkt : NULL);
            if (ret == AVERROR(EAGAIN)) {
                unsigned long long frames_before_drain = decoded_frames;
                ret = receive_available_frames(
                    dec, frame, dump_path, target_frame, frame_count,
                    &decoded_frames, &processed_frames, &exit_status);
                if (processed_frames >= frame_count)
                    break;
                if (ret == AVERROR_EOF) {
                    decoder_eof = 1;
                    continue;
                }
                if (ret != AVERROR(EAGAIN)) {
                    log_av_error("Draining decoder before retrying packet", ret);
                    goto cleanup;
                }
                if (decoded_frames == frames_before_drain) {
                    fprintf(stderr,
                            "Decoder send/receive made no progress after EAGAIN\n");
                    goto cleanup;
                }
                continue;
            }
            if (ret < 0 && !(flush_requested && ret == AVERROR_EOF)) {
                log_av_error(packet_pending ? "Sending packet" : "Flushing decoder", ret);
                goto cleanup;
            }

            if (packet_pending) {
                av_packet_unref(pkt);
                packet_pending = 0;
            } else {
                flush_sent = 1;
                flush_requested = 0;
                if (ret == AVERROR_EOF)
                    decoder_eof = 1;
            }
        }

        if (decoder_eof || processed_frames >= frame_count)
            break;

        ret = receive_available_frames(
            dec, frame, dump_path, target_frame, frame_count,
            &decoded_frames, &processed_frames, &exit_status);
        if (processed_frames >= frame_count)
            break;
        if (ret == AVERROR_EOF) {
            decoder_eof = 1;
        } else if (ret == AVERROR(EAGAIN)) {
            if (flush_sent) {
                fprintf(stderr,
                        "Decoder requested more input after flush was accepted\n");
                goto cleanup;
            }
        } else if (ret < 0) {
            log_av_error("Receiving decoded frame", ret);
            goto cleanup;
        }
    }

    if (processed_frames != frame_count) {
        fprintf(stderr, "Requested %llu frames starting at %llu, but only %llu were available\n",
                frame_count, target_frame, processed_frames);
        goto cleanup;
    }
    printf("\nSynchronization test passed: %llu/%llu frames imported, rendered, and completed\n",
           processed_frames, frame_count);
    exit_status = 0;

cleanup:
    av_frame_free(&frame);
    av_packet_free(&pkt);
    avcodec_free_context(&dec);
    av_buffer_unref(&device);
    avformat_close_input(&fmt);

    return exit_status;
}
