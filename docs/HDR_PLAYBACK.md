# HDR playback

Glimpse 0.5.0 includes HDR-capable VAAPI presentation in its default hardware-rendering path. No separate `vaapi-hdr` opt-in is required: the default `vaapi-dmabuf` feature includes NV12 and P010 DMA-BUF presentation, and the compatibility feature name `vaapi-hdr` aliases `vaapi-dmabuf`.

HDR presentation requires the complete system path to support the decoded format and its color information:

- GTK 4.16 or newer, with the GTK DMA-BUF APIs used by Glimpse
- FFmpeg 9.0 built with VAAPI support
- libdrm development headers and a working VAAPI render node
- A GPU/driver that can decode the source to a supported P010 surface
- A Wayland compositor and display path that accept the P010 DMA-BUF format/modifier and GTK color state

## Build and run

The DMA-BUF renderer is enabled by default:

```sh
cargo build -p glimpse
cargo run -p glimpse -- "/path/to/HDR10-video.mkv"
```

To explicitly enable the compatibility feature name in a non-default feature configuration:

```sh
cargo build -p glimpse --features vaapi-hdr
```

To force software decoding for troubleshooting, use `GLIMPSE_VIDEO_DECODER=software`. This selects the software decoder and RGBA/GTK texture presentation path; it is not the HDR-preserving path.

## HDR format and color handling

The hardware presentation path supports P010 frames and constructs GTK color state from the frame's color metadata. The HDR10 path expects the source to carry the relevant color tags, including:

- BT.2020 primaries
- SMPTE ST 2084 (PQ) transfer
- BT.2020 non-constant-luminance matrix
- Limited range

The GTK bridge validates the VAAPI frame, maps it to a DRM PRIME descriptor, checks the two-plane P010 layout, and imports the DMA-BUF planes. It retains the mapped DRM PRIME frame until GDK releases the texture, preventing the underlying VAAPI surface from being recycled while GTK is still sampling it.

The RGBA compatibility fallback is intended to preserve playback when DMA-BUF presentation fails; it is not equivalent to the hardware presentation path for HDR. The fallback downloads and converts frames to RGBA and does not currently preserve the full HDR color state. HDR fidelity therefore depends on the P010 DMA-BUF path being available.

Glimpse communicates supported CICP colorimetry to GTK. HDR10 mastering-display and content-light metadata are not currently forwarded to the compositor. The compositor, display configuration, and output capabilities also affect the final displayed result.

## Troubleshooting and current limitations

If hardware decoding or P010 import is unavailable, Glimpse may fall back to software decoding or RGBA presentation to keep playback working. Use `GLIMPSE_VIDEO_DEBUG=1` to inspect decoder selection, negotiated pixel formats, and presentation diagnostics. Set `GLIMPSE_VIDEO_DECODER=software` to compare against the compatibility path.

DMA-BUF import and `GtkGraphicsOffload` do not guarantee end-to-end zero-copy or direct scanout. The compositor may still need to composite the video surface, particularly when rotation, scaling, subtitles, or other overlays are active.

The Vulkan/libplacebo renderer remains future work; the production 0.5.0 path is VAAPI/DRM PRIME DMA-BUF presentation through GTK, with RGBA/GTK fallback.