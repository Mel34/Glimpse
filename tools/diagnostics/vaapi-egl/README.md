# VAAPI → DRM PRIME → EGL diagnostic

This standalone diagnostic proves the initial hardware-video presentation
path independently of the Glimpse application:

1. Decode a video frame using an FFmpeg decoder with VAAPI support.
2. Map the VAAPI frame directly to a DRM PRIME descriptor using
   `AV_HWFRAME_MAP_READ | AV_HWFRAME_MAP_DIRECT`.
3. Validate the DRM descriptor and classify its layer formats.
4. Query EGL DMA-BUF format and modifier support for recognized formats.
5. Import supported DMA-BUF layers as EGL images and bind them to OpenGL
   textures.
6. Render NV12 through a shader to an RGBA8 framebuffer, or inspect 10-bit
   texture samples through an RGBA32F framebuffer.
7. Insert an OpenGL sync fence after rendering and wait for GPU completion\n   before readback or releasing imported frame resources.\n8. Read framebuffer data back for diagnostic validation.

The readback is deliberately diagnostic-only. It is not part of the intended
zero-copy playback path.

## Scope and limitations

This is a diagnostic proof of concept, not the production renderer. Decoder
selection prefers FFmpeg's default decoder when it supports VAAPI; otherwise,
it searches the available decoders for one exposing VAAPI device-context
support. This allows codecs such as AV1 to use the native decoder when the
default decoder (for example, `libdav1d`) cannot produce VAAPI frames.

The diagnostic currently recognizes two DRM descriptor layouts:

- **8-bit NV12:** one object, two layers (`R8` and `GR88`), one plane per
  layer. This layout is imported and passed through the NV12 shader.
- **10-bit 4:2:0:** one object, two layers (`R16` and `GR32`), one plane per
  layer. This layout is imported and its samples are inspected.

The diagnostic requires:

- An FFmpeg decoder exposing VAAPI device-context support for the input codec.
- The render node `/dev/dri/renderD128`.
- EGL DMA-BUF import and modifier-query extensions.
- An OpenGL 3.3 core context on a hardware renderer.

Software renderers such as llvmpipe and softpipe are explicitly rejected.

The EGL capability query reports whether the actual DRM modifier used by each
frame layer is advertised for its format. The subsequent EGL import is the
actual import test; an advertised format alone does not guarantee that a
particular DMA-BUF can be imported.

Descriptors with malformed object or plane metadata are rejected. Valid
descriptors outside the two recognized layouts are rejected before the EGL
capability query and import are attempted. There is currently no software
decoding or pixel-conversion fallback for unsupported layouts.

The 10-bit path imports the layers as EGL images and OpenGL textures, then
samples them into an `RGBA32F` framebuffer. The diagnostic reconstructs
16-bit sample values and reports their ranges and the proportion of values
whose low six bits are zero. It also writes planar `yuv420p10le` samples for
comparison with a software-decoded reference.

This 10-bit path validates import and sample preservation, not color
conversion or final rendered output. The NV12 shader still uses fixed BT.709
limited-range conversion constants. Although the diagnostic prints each
frame's colorspace, range, and chroma-location metadata, it does not yet use
those values to configure the shader. Dynamic color metadata, additional DRM
layouts, fallback handling, synchronization, sustained-playback lifetime
testing, and performance measurement remain later work.

## Arch Linux prerequisites

Install the compiler and development packages:

```bash
sudo pacman -S --needed base-devel ffmpeg libva-mesa-driver mesa libdrm pkgconf
```

The build uses the `libavformat`, `libavcodec`, `libavutil`, `egl`, `gl`, and
`libdrm` pkg-config modules.

The user running the diagnostic must have access to the DRM render node.

## Build

From the Glimpse repository root:

```bash
cc -Wall -Wextra -Werror -O2 \
    tools/diagnostics/vaapi-egl/inspect.c \
    -o /tmp/glimpse-stage2-inspect \
    $(pkg-config --cflags --libs libavformat libavcodec libavutil egl gl libdrm)
```

## Layout validation self-test

The diagnostic includes a self-test for DRM descriptor validation and
supported-layout classification. It does not require a video file, an
accessible render node, or a working EGL implementation:

```bash
/tmp/glimpse-stage2-inspect --self-test-layout-validation
```

It checks that NV12 (`R8` + `GR88`) and 10-bit (`R16` + `GR32`) layouts are
recognized, an unsupported fourcc is rejected, and malformed plane metadata
is rejected.

Unsupported layouts are not passed to the EGL capability query or import
path. The diagnostic reports the unsupported layout and exits with status
`2`. This is deliberate rejection, not a software-decoding fallback.

## Run

Pass a video file, an optional output path, and an optional decoded frame
index:

```bash
/tmp/glimpse-stage2-inspect \
    "$HOME/Videos/Clips/Gungun.mp4" \
    /tmp/glimpse-stage2-gpu.rgba \
    0
```

If the output path is omitted, the diagnostic writes to
`/tmp/glimpse-stage-gpu-output.bin`.

The contents depend on the detected layout:

- NV12: tightly packed RGBA8 framebuffer capture (`width × height × 4`
  bytes).
- 10-bit: planar `yuv420p10le` samples, stored as 16-bit little-endian values
  (`width × height × 3` bytes).

The 10-bit output is not an RGBA image. Choose a filename that reflects the
format when passing an explicit output path. Both output formats preserve
OpenGL framebuffer readback row order.

## Generate a software reference

For the validated 720×720 BT.709 limited-range test clip:

```bash
ffmpeg -hide_banner -loglevel error \
    -hwaccel none \
    -i "$HOME/Videos/Clips/Gungun.mp4" \
    -map 0:v:0 -an -sn -dn \
    -frames:v 1 \
    -vf "scale=in_color_matrix=bt709:out_color_matrix=bt709:in_range=tv:out_range=pc:in_chroma_loc=left:out_chroma_loc=left,format=rgb24" \
    -f rawvideo \
    -y /tmp/glimpse-stage1-reference.rgb24
```

The reference and GPU capture must represent the same decoded frame. The
comparison script checks dimensions through the raw file sizes; it cannot
independently establish frame identity.

## Compare

The comparison script uses only the Python standard library:

```bash
python3 tools/diagnostics/vaapi-egl/compare.py \
    /tmp/glimpse-stage2-gpu.rgba \
    /tmp/glimpse-stage1-reference.rgb24
```

Both framebuffer orientations are compared. This is important because
OpenGL readback row order and the orientation of the sampled EGL textures
must be established empirically for the actual pipeline.

## Compare 10-bit samples

For a 10-bit frame, generate a software-decoded `yuv420p10le` reference for
the same frame index selected by the diagnostic. For example, to compare
frame 1000:

```bash
VIDEO="$HOME/Videos/The Expanse/The Expanse S01/The Expanse S01E07 Windmills (1080p x265 10bit Joy).mkv"

ffmpeg -hide_banner -loglevel error \
    -hwaccel none \
    -i "$VIDEO" \
    -map 0:v:0 -an -sn -dn \
    -vf "select=eq(n\\,1000),format=yuv420p10le" \
    -fps_mode passthrough -frames:v 1 \
    -f rawvideo -y /tmp/glimpse-stage2-reference.yuv
```

Run the diagnostic with the same frame index and an explicit output path:

```bash
/tmp/glimpse-stage2-inspect \
    "$VIDEO" \
    /tmp/glimpse-stage2-gpu.yuv \
    1000
```

Compare the planar samples:

```bash
python3 tools/diagnostics/vaapi-egl/compare.py \
    /tmp/glimpse-stage2-gpu.yuv \
    /tmp/glimpse-stage2-reference.yuv \
    --format yuv420p10le \
    --width 1920 \
    --height 1080
```

The 10-bit comparison reports per-plane MAE, maximum error, and exact-match
percentage for both framebuffer orientations. It compares sample values,
not RGB color conversion.

## Stage 1 validation record

Validated on:

- GPU: AMD Radeon RX 6700 XT (Navi 22).
- Driver: Mesa radeonsi.
- Decoder: FFmpeg H.264 VAAPI.
- Input: H.264 High, 720×720, 4:2:0, BT.709, limited range, left chroma
  location.
- DRM layout: one object, two layers (`R8` and `GR88`).
- EGL: 1.5, DMA-BUF import with modifiers.
- OpenGL: hardware renderer, AMD Radeon RX 6700 XT.

Observed results:

- Direct VAAPI-to-DRM PRIME mapping succeeded.
- Both DMA-BUF layers imported and became GL textures.
- Shader rendering produced a non-empty RGBA8 framebuffer.
- Software-reference comparison, using the captured row order, produced:

| Metric | Result |
|---|---:|
| Overall RGB mean absolute error | 1.1339 / 255 |
| Red MAE | 1.0295 |
| Green MAE | 1.3728 |
| Blue MAE | 0.9993 |
| P95 channel error | 2 |
| P99 channel error | 3 |
| Maximum channel error | 31 |
| Channel samples differing by more than 5 | 0.2971% |
| Channel samples differing by more than 10 | 0.0442% |

Explicitly selecting left chroma location in the FFmpeg reference produced
a byte-identical reference to the default conversion for this test.

These measurements establish close agreement for this particular format,
GPU, driver, shader, and input. They do not establish correctness for other
formats, color spaces, modifiers, GPUs, or drivers.

## Stage 2 format and metadata validation record

Validated on the same AMD Radeon RX 6700 XT with Mesa radeonsi:

| Codec / profile | Input format | DRM layers | Result |
|---|---|---|---|
| H.264 High | 8-bit 4:2:0 | `R8` + `GR88` | EGL import and NV12 shader rendering |
| AV1 Main | 8-bit 4:2:0 | `R8` + `GR88` | EGL import and NV12 shader smoke test (non-empty framebuffer; no color-accuracy comparison) |
| HEVC Main10 | 10-bit 4:2:0 | `R16` + `GR32` | EGL import and sample inspection |
| VP9 Profile 2 | 10-bit 4:2:0 | `R16` + `GR32` | EGL import and sample inspection |

All four tested layouts used one DRM object with two one-plane layers and
the modifier `0x0200000018801b03`. EGL advertised that modifier for each
corresponding layer format, with `external_only=no`, on this GPU and driver.

The decoded frame metadata reported by FFmpeg was:

| Test input | `colorspace` | `range` | `chroma_location` |
|---|---:|---:|---:|
| H.264 | 1 (BT.709) | 1 (limited) | 1 (left) |
| AV1 | 1 (BT.709) | 1 (limited) | 0 (unspecified) |
| HEVC Main10 | 2 (unspecified) | 1 (limited) | 1 (left) |
| VP9 Profile 2 | 5 (BT.470BG) | 1 (limited) | 1 (left) |

These are FFmpeg enum values. The diagnostic reports the metadata but does
not yet apply it to color conversion.

The HEVC Main10 frame 1000 and VP9 Profile 2 frame 0 both produced exact
10-bit sample matches against their software-decoded references in the
captured row order:

| Test | Y samples | U samples | V samples |
|---|---:|---:|---:|
| HEVC Main10, frame 1000 | 100% exact | 100% exact | 100% exact |
| VP9 Profile 2, frame 0 | 100% exact | 100% exact | 100% exact |

The HEVC comparison covered 2,592,000 samples; the VP9 comparison covered
259,200 samples. The vertically flipped comparisons did not match, confirming
that the captured row order is the matching orientation for these tests.

These results validate the tested decoder outputs, DRM layouts, modifier,
EGL imports, and sample preservation on this specific hardware and software
stack. They do not establish support for other layouts, modifiers, GPUs, or
drivers, nor do they validate 10-bit color conversion.

## Stage boundary

Stage 1 validated image correctness for the initial NV12 path. Stage 2
validated the two recognized DRM layouts, their EGL imports, and the
preservation of tested 10-bit samples while documenting frame metadata.
Successful decode and DRM PRIME mapping of a format do not by themselves
imply that EGL import or shader rendering supports it.


## Stage 4 synchronization check

The renderer inserts an OpenGL 3.2 sync object immediately after each frame's draw call, flushes the command stream, and waits for that fence to signal before framebuffer readback and before the EGL images, GL textures, and mapped DRM frame are released. Fence creation or wait failures fail the frame.

To exercise repeated frame handoff, request a bounded sequence:

```bash
/tmp/glimpse-stage-inspect \
    "$HOME/Videos/Clips/Gungun.mp4" \
    /tmp/glimpse-stage4-frame.rgba \
    0 \
    30
```

This processes 30 consecutive decoded frames beginning at index 0. Each capture is written to a separate `.frame-N` file. The test reports success only when every requested frame has been decoded, mapped, imported, rendered, and GPU-completed.

This is a sequential completion/lifetime test. It deliberately waits for each frame's GPU fence before advancing, so it does not establish safe concurrent decoder/GPU overlap or prove asynchronous multi-frame playback.
