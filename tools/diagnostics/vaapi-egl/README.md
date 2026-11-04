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

The readback is deliberately diagnostic-only. It is not part of the intended
zero-copy playback path.

## Scope and limitations

This is a Stage 1 proof of concept, not the production renderer. It currently
expects:

- A VAAPI-capable FFmpeg decoder.
- The render node `/dev/dri/renderD128`.
- A DRM PRIME descriptor with one object and two layers.
- NV12 represented by `R8` and `GR88` layers, each with one plane.
- An EGL implementation supporting DMA-BUF import and modifiers.
- An OpenGL 3.3 core context on a hardware renderer.
- BT.709 limited-range input.

The shader currently uses fixed BT.709 limited-range conversion constants.
Format discovery, additional layouts, dynamic color metadata, fallback
handling, synchronization, sustained-playback lifetime testing, and
performance measurement are later work.

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
    -o /tmp/glimpse-stage1-inspect \
    $(pkg-config --cflags --libs libavformat libavcodec libavutil egl gl libdrm)
```

## Run

Pass a video file and, optionally, the destination for the raw RGBA capture:

```bash
/tmp/glimpse-stage1-inspect \
    "$HOME/Videos/Clips/Gungun.mp4" \
    /tmp/glimpse-stage1-gpu.rgba
```

If the output path is omitted, the diagnostic writes to
`/tmp/glimpse-stage1-gpu.rgba`.

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

Stage 1 validates image correctness for the initial NV12 path. Stage 2 will
address format and metadata handling without destabilizing this known-good
baseline.
