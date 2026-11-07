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
7. Insert an OpenGL sync fence after rendering and wait for GPU completion
   before readback or releasing imported frame resources.
8. Read framebuffer data back for diagnostic validation.

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
descriptors outside the two recognized layouts are classified as requiring
the software fallback before EGL import. The diagnostic does not itself
perform software fallback; exit status 2 identifies a frame that the
production playback path should route through its existing software path.

The 10-bit path imports the R16/GR32 layers as EGL images and OpenGL textures.
It verifies sample preservation by writing planar `yuv420p10le` data, then
renders the same frame to an additional RGBA8 image. The RGBA output is written
to `OUTPUT.rgba`; the original output path continues to contain the planar
10-bit samples for exact comparison.

The color conversion shader now handles 8-bit NV12 and left-aligned 10-bit
R16/GR32 samples. It resolves FFmpeg frame metadata for supported matrix
coefficients (BT.601-family, BT.709, SMPTE 240M, and BT.2020 non-constant
luminance), full or limited range, and 4:2:0 chroma location. Unspecified
metadata uses documented defaults: BT.709 above 576 lines, BT.601 otherwise,
limited range, and left-sited chroma. Unsupported matrix/range metadata is
classified as requiring software fallback rather than rendered with guessed
coefficients.

The format/modifier capability query now classifies a frame as requiring
software fallback when its DRM format or explicit modifier is not advertised
for 2D texture import, including modifiers marked external-only. An actual
EGL import or shader-render failure is also reported as fallback-required.
The diagnostic remains a hardware-path validator, not the production
fallback implementation.

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
cc -Wall -Wextra -Werror -O2 -pthread \
    tools/diagnostics/vaapi-egl/inspect.c \
    -o /tmp/glimpse-stage6-inspect \
    $(pkg-config --cflags --libs libavformat libavcodec libavutil egl gl libdrm)
```

## Layout validation self-test

The diagnostic includes a self-test for DRM descriptor validation and
supported-layout classification. It does not require a video file, an
accessible render node, or a working EGL implementation:

```bash
/tmp/glimpse-stage6-inspect --self-test-layout-validation
/tmp/glimpse-stage6-inspect --self-test-color-conversion
```

The layout test checks that NV12 (`R8` + `GR88`) and 10-bit
(`R16` + `GR32`) layouts are recognized, an unsupported fourcc is rejected,
and malformed plane metadata is rejected. The color test covers BT.709 and
BT.2020 matrices, limited and full range, 8-bit and 10-bit code values,
unspecified-metadata defaults, and rejection of unsupported constant-
luminance BT.2020 metadata.

Unsupported layouts are not passed to the EGL capability query or import
path. The diagnostic reports the unsupported layout and exits with status
`2`. This is deliberate rejection, not a software-decoding fallback.

## Run

Pass a video file, an optional output path, and an optional decoded frame
index. The default output is a compact summary rather than per-frame logs:

```bash
/tmp/glimpse-stage6-inspect \
    "$HOME/Videos/Clips/Gungun.mp4" \
    /tmp/glimpse-stage2-gpu.rgba \
    0
```

If the output path is omitted, the diagnostic writes to
`/tmp/glimpse-stage-gpu-output.bin`.

### Output modes

The compact summary is the default. The mode flag, when used, must precede the
video path:

- `--summary`: compact result (also the default).
- `--verbose`: restore the detailed per-frame DRM, EGL, import, and pixel
  diagnostics.
- `--quiet`: print only a one-line success result. Failures still report
  diagnostic details on standard error.

For example:

```bash
/tmp/glimpse-stage6-inspect --verbose \
    "$HOME/Videos/Clips/Gungun.mp4" \
    /tmp/glimpse-stage-frame.rgba \
    0 30

/tmp/glimpse-stage6-inspect --quiet \
    "$HOME/Videos/Clips/Gungun.mp4" \
    /tmp/glimpse-stage-frame.rgba \
    0 30
```

The contents depend on the detected layout:

- NV12: tightly packed RGBA8 framebuffer capture (`width × height × 4`
  bytes).
- 10-bit: planar `yuv420p10le` samples at the requested output path, plus an
  RGBA8 render at `OUTPUT.rgba`.

The planar 10-bit output stores 16-bit little-endian samples
(`width × height × 3` bytes). Both outputs preserve OpenGL framebuffer
readback row order. Choose an explicit output filename that reflects the
planar YUV format.

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
/tmp/glimpse-stage6-inspect \
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
/tmp/glimpse-stage6-inspect \
    "$HOME/Videos/Clips/Gungun.mp4" \
    /tmp/glimpse-stage4-frame.rgba \
    0 \
    30
```

This processes 30 consecutive decoded frames beginning at index 0. Each capture is written to a separate `.frame-N` file. The test reports success only when every requested frame has been decoded, mapped, imported, rendered, and GPU-completed.

This is a sequential completion/lifetime test. It deliberately waits for each frame's GPU fence before advancing, so it does not establish safe concurrent decoder/GPU overlap or prove asynchronous multi-frame playback.

## Stage 6 production format and color handling

Stage 6 adds a reusable FFmpeg metadata resolver in
`tools/diagnostics/vaapi-egl/color.h`. It maps supported matrix coefficients,
range, bit depth, and chroma siting into shader parameters. The standalone
diagnostic uses those parameters for both NV12 and 10-bit R16/GR32 rendering.

Build, then run both pure metadata/layout self-tests:

```bash
/tmp/glimpse-stage6-inspect --self-test-layout-validation
/tmp/glimpse-stage6-inspect --self-test-color-conversion
```

Hardware validation should cover:

1. H.264 or AV1 NV12 with BT.709 limited range; compare the RGBA output with
   an FFmpeg software reference using matching matrix, range, and chroma
   location.
2. HEVC Main10 with its reported metadata; compare both the planar 10-bit
   sample dump and the new RGBA output against software decoding.
3. VP9 Profile 2 with its reported metadata; repeat both comparisons.
4. A frame with unspecified metadata, confirming the selected defaults are
   reported and applied.
5. A frame whose format/modifier cannot be imported as a 2D texture, confirming
   that it is classified as fallback-required rather than treated as a fatal
   format-conversion success.

Stage 6 does not connect the hardware path to the application playback
pipeline. It provides the reusable conversion policy and diagnostic evidence
needed before Stage 7 integration. Color correctness must be established by
software-reference comparisons on each supported format/metadata combination;
successful shader execution alone is not sufficient.

## Stage 5 bounded decode/render pipeline

The diagnostic now runs decode and rendering on separate threads:

- The producer thread exclusively owns the FFmpeg input, packet, decoder,
  VAAPI device references, and decode frame.
- The renderer thread owns all EGL/OpenGL work. It consumes cloned AVFrame
  references from a three-frame bounded queue.
- When the queue is full, the producer blocks until the renderer releases a
  slot. This bounds the number of decoded surfaces retained ahead of rendering.
- The renderer waits for each frame's OpenGL fence before releasing that
  frame's mapped DRM PRIME descriptor, EGL images, GL textures, and queued
  AVFrame reference.
- A renderer failure cancels the queue, wakes a producer blocked on queue
  capacity, joins the producer thread, and releases queued frame references.
  Decoder/input or queue synchronization failures close the queue; the renderer
  drains already queued frames and then reports the producer failure.

Build with pthread support using the command above. Then run a sustained
300-frame test:

    /tmp/glimpse-stage6-inspect \
        "$HOME/Videos/Clips/Gungun.mp4" \
        /tmp/glimpse-stage5-frame.rgba \
        0 \
        300

Each frame capture is written to a separate .frame-N file. The test passes
only when all requested frames have been decoded, queued, rendered, and
GPU-completed. If the input reaches end-of-stream before the requested count,
the producer reports the shortfall and the overall test fails after queued
frames have been released.

This pipeline permits CPU decoding to overlap with renderer/GPU work while
keeping at most three decoded frame references queued. It does not claim that
multiple frames are executing concurrently on the GPU, and it does not
integrate the pipeline into production playback. Sustained hardware testing,
failure-path testing, and memory/file-descriptor leak checks remain required
on the target AMD Radeon RX 6700 XT.
