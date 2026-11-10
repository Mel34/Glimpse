# GTK HDR / DMA-BUF diagnostic

This probe is a standalone Cargo workspace, deliberately isolated from
Glimpse's production workspace. It uses GTK 4.24-era Rust bindings
(gtk4/gdk4 0.9), while the production application remains on its existing
GTK 4.12 binding baseline (gtk4/gdk4 0.8) with libadwaita 0.6.

The isolation is required because Cargo permits only one crate in a dependency
graph to link the native `gtk-4` library. The production app and this probe
therefore have separate dependency graphs and lockfiles.

The probe allocates a 1920×1080, single-plane XRGB2101010 DMA-BUF through GBM,
fills it through GBM's CPU mapping API with a 10-bit Rec. 2100 PQ grayscale
ramp, imports it with `GdkDmabufTextureBuilder`, and tags it with the
Rec. 2100 PQ color state. It also creates a system-memory GDK texture from the
exact same pixel bytes and color state. The two textures are displayed side by
side.

This comparison separates pattern generation/color-state issues from
DMA-BUF sampling/import issues. It does not yet test FFmpeg decoder output,
YUV/P010 sampling, HDR mastering metadata, or direct scanout.

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
GDK_DEBUG=color-mgmt,dmabuf cargo run --manifest-path tools/diagnostics/hdr/Cargo.toml
```

The application prints the allocated format, dimensions, stride, modifier,
and packed first/last pixel values. If GBM cannot allocate or CPU-map the
linear XRGB2101010 buffer, or GTK cannot import the exported DMA-BUF, the
window displays the error instead of silently falling back.

## Wayland protocol tracing

```sh
GDK_DEBUG=color-mgmt,dmabuf WAYLAND_DEBUG=1 \
  cargo run --manifest-path tools/diagnostics/hdr/Cargo.toml \
  2>hdr-dmabuf-normal.log

GDK_DEBUG=hdr,color-mgmt,dmabuf WAYLAND_DEBUG=1 \
  cargo run --manifest-path tools/diagnostics/hdr/Cargo.toml \
  2>hdr-dmabuf-forced-hdr.log

grep -Ei 'wp_color_management_surface|wp_image_description|set_image_description|dmabuf|HDR' \
  hdr-dmabuf-normal.log hdr-dmabuf-forced-hdr.log
```

Compare the DMA-BUF image on the left with the byte-identical system-memory
reference on the right. If only the right side appears, PQ encoding and the
GDK color state are working; investigate DMA-BUF storage/import/sampling.
If both appear, compare their luminance steps and protocol image descriptions.

No production decoder, frame scheduler, subtitle, audio, or playback code is
changed by this probe.
