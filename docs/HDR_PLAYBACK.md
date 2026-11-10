# Experimental VAAPI HDR10 presentation

Glimpse has an opt-in VAAPI presentation path based on the verified GTK 4.24
P010 DMA-BUF import diagnostic.

The default build and playback path are unchanged. The experimental path is
enabled explicitly because continuous playback, seeking, surface-pool pressure,
and long-running decoder behavior still need validation.

## Build

The optional renderer requires:

- GTK 4.24 or newer
- FFmpeg 9.0 with VAAPI support
- libdrm development headers
- A working VAAPI render node
- A Wayland compositor exposing P010 DMA-BUF import and color management

Build Glimpse with the renderer enabled:

```sh
cargo build -p glimpse --features vaapi-hdr
```

## Run an HDR10 P010 file

```sh
GLIMPSE_VIDEO_DECODER=vaapi cargo run -p glimpse --features vaapi-hdr -- "/path/to/HDR10-video.mkv"
```

If FFmpeg does not select the intended render node automatically:

```sh
GLIMPSE_DRM_RENDER_NODE=/dev/dri/renderD128 \
GLIMPSE_VIDEO_DECODER=vaapi \
cargo run -p glimpse --features vaapi-hdr -- "/path/to/HDR10-video.mkv"
```

The opt-in decoder currently targets HDR10 frames with P010 output and these
frame color tags:

- BT.2020 primaries
- SMPTE ST 2084 (PQ) transfer
- BT.2020 non-constant-luminance matrix
- Limited range

The GTK bridge validates the VAAPI frame, maps it directly to a DRM PRIME
descriptor, checks the two-plane P010 layout and imports the original DMA-BUF
planes. It retains the mapped DRM PRIME frame until GDK releases the texture,
so the underlying VAAPI surface is not recycled while GTK is still sampling it.

The existing RGBA8 software path remains the default. Do not enable
`GLIMPSE_VIDEO_DECODER=vaapi` for SDR or non-P010 content yet: format-specific
fallback and broader color-state handling are not part of this first slice.

## Current scope

This is continuous playback integration, not yet a release-ready renderer.
The first validation pass should cover:

- sustained playback and decoder surface recycling
- seeking and reopening a file
- pause/resume and end-of-stream
- subtitle overlays
- resize and fullscreen transitions
- confirming SDR content still uses the default software path

The path currently communicates CICP colorimetry to GTK. HDR10 mastering
display and content-light metadata are not yet forwarded to the compositor.
Audio playback and the PipeWire pipeline are unchanged.
