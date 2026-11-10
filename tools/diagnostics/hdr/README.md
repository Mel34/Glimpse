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
ramp, and imports it with `GdkDmabufTextureBuilder`. It also creates a
system-memory texture from the exact same XRGB2101010 bytes and tags both
textures with the Rec. 2100 PQ color state.

GTK 4.24 added `GDK_MEMORY_XRGB2101010` as `GdkMemoryFormat` enum value 67.
gtk-rs 0.9.6 predates the v4_24 feature and does not name this variant, but its
forward-compatible hidden enum fallback passes unknown numeric values through
unchanged. The probe uses that fallback to compare the two paths without
changing the production dependency graph.

This byte-identical comparison separates DMA-BUF import/sampling from pixel
packing, PQ encoding, and color-state differences. It does not yet test FFmpeg
decoder output, YUV/P010 sampling, HDR mastering metadata, or direct scanout.

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

The application prints the allocated format, dimensions, stride, and
modifier. If GBM cannot allocate or CPU-map the linear XRGB2101010 buffer, or
GTK cannot import the exported DMA-BUF, the window displays the error instead
of silently falling back.

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
reference on the right. If the right appears and the left remains black, the
failure is isolated to DMA-BUF storage/import/sampling. If both appear, the
packed format and PQ state are accepted by GDK's memory-texture path.

No production decoder, frame scheduler, subtitle, audio, or playback code is
changed by this probe.
