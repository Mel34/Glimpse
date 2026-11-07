#ifndef GLIMPSE_VAAPI_EGL_COLOR_H
#define GLIMPSE_VAAPI_EGL_COLOR_H

#include <libavutil/pixfmt.h>

typedef struct {
    float kr;
    float kb;
    float y_offset;
    float y_scale;
    float chroma_offset;
    float chroma_scale;
    float sample_scale;
    float chroma_x;
    float chroma_y;
    int bit_depth;
    int used_default_colorspace;
    int used_default_range;
    int used_default_chroma_location;
} GlimpseColorConversion;

/*
 * Resolve FFmpeg frame metadata into normalized shader parameters.
 *
 * Unspecified metadata follows common video defaults:
 * - BT.709 for frames taller than 576 lines, BT.601 otherwise.
 * - MPEG/limited range.
 * - Left-sited 4:2:0 chroma.
 *
 * Unsupported matrix coefficients are rejected rather than silently
 * producing incorrectly colored output.
 */
static inline int glimpse_color_conversion_resolve(
    enum AVColorSpace colorspace,
    enum AVColorRange range,
    enum AVChromaLocation chroma_location,
    int width,
    int height,
    int bit_depth,
    GlimpseColorConversion *out)
{
    if (!out || width <= 0 || height <= 0 ||
        (bit_depth != 8 && bit_depth != 10))
        return -1;

    *out = (GlimpseColorConversion){ 0 };
    out->bit_depth = bit_depth;
    out->sample_scale = bit_depth == 8 ? 255.0f : (65535.0f / 64.0f);

    if (colorspace == AVCOL_SPC_UNSPECIFIED) {
        colorspace = height > 576
            ? AVCOL_SPC_BT709 : AVCOL_SPC_SMPTE170M;
        out->used_default_colorspace = 1;
    }

    switch (colorspace) {
    case AVCOL_SPC_BT709:
        out->kr = 0.2126f;
        out->kb = 0.0722f;
        break;
    case AVCOL_SPC_FCC:
        out->kr = 0.30f;
        out->kb = 0.11f;
        break;
    case AVCOL_SPC_BT470BG:
    case AVCOL_SPC_SMPTE170M:
        out->kr = 0.2990f;
        out->kb = 0.1140f;
        break;
    case AVCOL_SPC_SMPTE240M:
        out->kr = 0.212f;
        out->kb = 0.087f;
        break;
    case AVCOL_SPC_BT2020_NCL:
        out->kr = 0.2627f;
        out->kb = 0.0593f;
        break;
    default:
        return -1;
    }

    if (range == AVCOL_RANGE_UNSPECIFIED) {
        range = AVCOL_RANGE_MPEG;
        out->used_default_range = 1;
    }

    float depth_scale = bit_depth == 8 ? 1.0f : 4.0f;
    if (range == AVCOL_RANGE_MPEG) {
        out->y_offset = 16.0f * depth_scale;
        out->y_scale = 219.0f * depth_scale;
        out->chroma_offset = 128.0f * depth_scale;
        out->chroma_scale = 224.0f * depth_scale;
    } else if (range == AVCOL_RANGE_JPEG) {
        float maximum = bit_depth == 8 ? 255.0f : 1023.0f;
        out->y_offset = 0.0f;
        out->y_scale = maximum;
        out->chroma_offset = (maximum + 1.0f) * 0.5f;
        out->chroma_scale = maximum;
    } else {
        return -1;
    }

    if (chroma_location == AVCHROMA_LOC_UNSPECIFIED) {
        chroma_location = AVCHROMA_LOC_LEFT;
        out->used_default_chroma_location = 1;
    }

    switch (chroma_location) {
    case AVCHROMA_LOC_LEFT:
        out->chroma_x = 0.0f;
        out->chroma_y = 0.5f;
        break;
    case AVCHROMA_LOC_CENTER:
        out->chroma_x = 0.5f;
        out->chroma_y = 0.5f;
        break;
    case AVCHROMA_LOC_TOPLEFT:
        out->chroma_x = 0.0f;
        out->chroma_y = 0.0f;
        break;
    case AVCHROMA_LOC_TOP:
        out->chroma_x = 0.5f;
        out->chroma_y = 0.0f;
        break;
    case AVCHROMA_LOC_BOTTOMLEFT:
        out->chroma_x = 0.0f;
        out->chroma_y = 1.0f;
        break;
    case AVCHROMA_LOC_BOTTOM:
        out->chroma_x = 0.5f;
        out->chroma_y = 1.0f;
        break;
    default:
        return -1;
    }

    return 0;
}

#endif
