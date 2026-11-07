# GTK DMA-BUF diagnostic

A standalone proof of concept for presenting a VAAPI-decoded frame through
GTK's `GdkDmabufTextureBuilder`.

This diagnostic is intentionally separate from the Glimpse player and from
the existing `vaapi-egl` diagnostic.

## Current scope

- Decodes the first video frame using a VAAPI hardware decoder.
- Maps that frame to DRM PRIME using a direct read mapping.
- Accepts only the validated single-object, two-layer `R8` + `GR88` layout.
- Checks that the active GDK display advertises the exact `DRM_FORMAT_NV12`
  and modifier pair.
- Builds a two-plane GDK DMA-BUF texture and displays it in a GTK window.
- Keeps the mapped FFmpeg frame alive until GDK releases the texture.

P010 and other layouts are deliberately rejected in this first pass. A
successful import demonstrates DMA-BUF texture creation and presentation;
it does not establish colour accuracy.

## Build

From the repository root:

```sh
cc -std=c11 -O2 -Wall -Wextra \
  tools/diagnostics/gtk-dmabuf/inspect.c \
  -o /tmp/glimpse-gtk-dmabuf \
  $(pkg-config --cflags --libs gtk4 libavformat libavcodec libavutil libdrm)
```

## Run

```sh
/tmp/glimpse-gtk-dmabuf /path/to/video
```

The default VAAPI render node is `/dev/dri/renderD128`. Override it with:

```sh
GLIMPSE_DRI_RENDER_NODE=/dev/dri/renderD128 \
  /tmp/glimpse-gtk-dmabuf /path/to/video
```

The application must run in a graphical session supported by the active GTK
backend. GDK's advertised DMA-BUF formats and modifiers are queried from that
active display at runtime.

## Limitations

- The first frame is decoded synchronously during GTK activation.
- Only NV12 with the exact advertised modifier is attempted.
- The expected FFmpeg DRM descriptor is one object with two one-plane layers:
  `DRM_FORMAT_R8` followed by `DRM_FORMAT_GR88`.
- No colour-state conversion is configured. Treat the displayed colours as
  diagnostic output, not as a colour-correct playback result.
- This is an integration experiment, not production playback code.


## Glimpse player integration preview

The player has an opt-in first-frame integration path. It uses the Rust
FFmpeg FFI to decode a VAAPI frame, maps it to DRM PRIME, validates the
single-object NV12 layout, and imports it through gtk-rs' GDK DMA-BUF builder.
The mapped FFmpeg frame remains alive until GDK releases the texture.

Build and launch Glimpse with the feature enabled:

```sh
cargo run -p glimpse --features vaapi-dmabuf-preview
```

To activate the preview for files opened from the file dialog or drag-and-drop:

```sh
GLIMPSE_VAAPI_DMABUF_PREVIEW=1 \
GLIMPSE_DRI_RENDER_NODE=/dev/dri/renderD128 \
  cargo run -p glimpse --features vaapi-dmabuf-preview
```

This is intentionally a still-frame diagnostic, not continuous playback. The
preview decodes synchronously and can temporarily block the GTK interface.
When import fails, Glimpse falls back to its normal GTK MediaFile playback.
On successful import, the picture displays the first frame and MediaFile
playback remains paused. Colour accuracy is not yet validated.
