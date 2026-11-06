# VAAPI → DRM PRIME → EGL diagnostic

This standalone diagnostic proves the initial hardware-video presentation
path independently of the Glimpse application:

1. Decode a video frame using FFmpeg's VAAPI hardware decoder.
2. Map the VAAPI frame directly to a DRM PRIME descriptor using
   `AV_HWFRAME_MAP_READ | AV_HWFRAME_MAP_DIRECT`.
3. Import the descriptor's DMA-BUF layers as EGL images.
4. Bind those images to OpenGL textures.
5. Sample the imported NV12 planes in a shader and render to an RGBA8
   framebuffer.
6. Read the framebuffer back for diagnostic validation.
7. For supported 10-bit 4:2:0 layouts, inspect imported texture samples to
   determine their effective bit alignment.

The readback is deliberately diagnostic-only. It is not part of the intended
zero-copy playback path.

## Scope and limitations

This is a diagnostic proof of concept, not the production renderer. Decoder
selection prefers FFmpeg's default decoder when it supports VAAPI; otherwise,
it searches the available decoders for one exposing VAAPI device-context
support. This allows codecs such as AV1 to use the native decoder when the
default decoder (for example, `libdav1d`) cannot produce VAAPI frames.

The diagnostic can inspect VAAPI-to-DRM PRIME mapping and EGL import for
additional formats. Its validated shader-rendering path currently expects:

- An FFmpeg decoder exposing VAAPI device-context support for the input codec.
- The render node `/dev/dri/renderD128`.
- A DRM PRIME descriptor with one object and two layers.
- NV12 represented by `R8` and `GR88` layers, each with one plane.
- An EGL implementation supporting DMA-BUF import and modifiers.
- An OpenGL 3.3 core context on a hardware renderer.
- BT.709 limited-range input.

The diagnostic also recognizes 10-bit 4:2:0 frames represented by `R16` and
`GR32` layers. It imports these layers as EGL images and OpenGL textures, then
samples them into an `RGBA32F` framebuffer for diagnostic readback. The
reported sample ranges and proportions of values with the low six bits zero
help assess whether the decoded 10-bit samples are left- or right-aligned in
their 16-bit storage.

This 10-bit path validates import and sample inspection only. It does not yet
perform 10-bit color conversion or validate rendered output. The NV12 shader
still uses fixed BT.709 limited-range conversion constants. Dynamic color
metadata, additional DRM layouts, fallback handling, synchronization,
sustained-playback lifetime testing, and performance measurement remain later
work.

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
`/tmp/glimpse-stage1-gpu.rgba`. The RGBA8 capture is produced only when the
frame uses the NV12 EGL rendering path. The 10-bit inspection path performs
its own temporary `RGBA32F` framebuffer readback and reports sample statistics;
it does not write an RGBA8 capture.

The capture is tightly packed RGBA8 in the framebuffer's readback row order.
Its expected size is `width × height × 4` bytes.

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
    /tmp/glimpse-stage1-gpu.rgba \
    /tmp/glimpse-stage1-reference.rgb24
```

Both framebuffer orientations are compared. This is important because
OpenGL readback row order and the orientation of the sampled EGL textures
must be established empirically for the actual pipeline.

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

## Stage boundary

Stage 1 validated image correctness for the initial NV12 path. Stage 2 is
expanding format and metadata inspection while preserving that known-good
baseline. Successful decode and DRM PRIME mapping of a format do not yet imply
that EGL import or shader rendering supports it.
