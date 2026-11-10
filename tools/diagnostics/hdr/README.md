# GTK HDR / DMA-BUF diagnostic

This probe is a standalone Cargo workspace, deliberately isolated from
Glimpse's production workspace. It uses GTK 4.16-era Rust bindings
(gtk4/gdk4 0.9), while the production application remains on its existing
GTK 4.12 binding baseline (gtk4/gdk4 0.8) with libadwaita 0.6.

The isolation is required because Cargo permits only one crate in a dependency
graph to link the native `gtk-4` library. The production app and this probe
therefore have separate dependency graphs and lockfiles.

The probe allocates a 1920×1080, single-plane XRGB2101010 DMA-BUF through GBM
using a linear renderable allocation, fills it through GBM's CPU mapping API
with a 10-bit Rec. 2100 PQ grayscale ramp, imports it with
`GdkDmabufTextureBuilder`, tags the texture with the Rec. 2100 PQ color state,
and displays it through `GtkGraphicsOffload`.

This exercises a GPU-backed RGB DMA-BUF import and the Wayland surface color
description. It does not yet test FFmpeg decoder output, YUV/P010 sampling,
HDR mastering metadata, or direct scanout.

## Requirements

- GTK 4.24 or newer at runtime
- A Wayland session with the color-management protocol
- GBM development library (`libgbm`) and a usable DRM render node
- Permission to open the selected DRM node read/write

The default DRM node is `/dev/dri/renderD128`. Override it with
`GLIMPSE_DRM_DEVICE` if the render node has a different path.

## Run

From the repository root:

```sh
GDK_DEBUG=color-mgmt,offload,dmabuf \
  cargo run --manifest-path tools/diagnostics/hdr/Cargo.toml
```

The application prints the allocated format, dimensions, stride, and modifier.
If GBM cannot allocate or CPU-map the linear XRGB2101010 buffer, or GTK cannot
import the exported DMA-BUF, the window displays the error instead of silently
falling back to a system-memory texture.

For a forced-HDR comparison:

```sh
GDK_DEBUG=hdr,color-mgmt,offload,dmabuf \
  cargo run --manifest-path tools/diagnostics/hdr/Cargo.toml
```

The `hdr` switch is a diagnostic override, not a production setting. The
previous system-memory probe already showed that normal GTK color management
submits a BT.2020/PQ image description to Mutter; compare this DMA-BUF run to
that baseline.

## Wayland protocol tracing

```sh
GDK_DEBUG=color-mgmt,offload,dmabuf WAYLAND_DEBUG=1 \
  cargo run --manifest-path tools/diagnostics/hdr/Cargo.toml \
  2>hdr-dmabuf-normal.log

GDK_DEBUG=hdr,color-mgmt,offload,dmabuf WAYLAND_DEBUG=1 \
  cargo run --manifest-path tools/diagnostics/hdr/Cargo.toml \
  2>hdr-dmabuf-forced-hdr.log

grep -Ei 'wp_color_management_surface|wp_image_description|set_image_description|offload|dmabuf|HDR' \
  hdr-dmabuf-normal.log hdr-dmabuf-forced-hdr.log
```

Look for the same PQ surface-description transaction as the first probe and
for GDK diagnostics confirming DMA-BUF import/offload. Capability queries by
Mesa alone are not proof that the texture was imported or offloaded.

Run windowed first, then fullscreen. Compare the grayscale bands with an HDR
reference in mpv. The 203 cd/m² patch is the reference-white checkpoint;
higher patches should remain distinguishable up to the display's capabilities.

## Scope and interpretation

The buffer uses 10-bit packed RGB to keep the first DMA-BUF test simple and
avoid mixing YUV range/matrix/chroma-siting questions into this stage. It is
allocated for rendering and CPU-mapped; it does not rely on GBM's optional
WRITE usage, which is not guaranteed for arbitrary format/usage combinations. The
next diagnostic step, if this succeeds, is to import a P010 DMA-BUF and
validate the video-like YUV path. That will be closer to the VAAPI decoder
output Glimpse intends to present.

No production decoder, frame scheduler, subtitle, audio, or playback code is
changed by this probe.
