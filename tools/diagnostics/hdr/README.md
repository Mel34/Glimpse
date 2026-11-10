# GTK HDR / DMA-BUF diagnostic

This probe is a standalone Cargo workspace, deliberately isolated from
Glimpse's production workspace. It uses GTK 4.24-era Rust bindings
(gtk4/gdk4 0.9), while the production application remains on its existing
GTK 4.12 binding baseline (gtk4/gdk4 0.8) with libadwaita 0.6.

The isolation is required because Cargo permits only one crate in a dependency
graph to link the native `gtk-4` library. The production app and this probe
therefore have separate dependency graphs and lockfiles.

## What it displays

The window has three columns:

1. **PQ DMA-BUF** — a GBM-allocated, linear XRGB2101010 buffer tagged with
   Rec. 2100 PQ.
2. **PQ system memory** — a GDK memory texture made from the exact same
   XRGB2101010 bytes and the same PQ color state.
3. **PQ ARGB DMA-BUF** — the same 10-bit PQ ramp in ARGB2101010, with
   explicit opaque alpha rather than unused X/padding bits.
4. **SDR DMA-BUF control** — a separate GBM-allocated XRGB8888 buffer using
   GDK's default sRGB color state.

The first two columns isolate the DMA-BUF path from pixel packing, PQ
encoding, and color-state differences. The third checks whether GDK can
import and sample a conventional 8-bit RGB DMA-BUF at all.

GTK 4.24 added `GDK_MEMORY_XRGB2101010` as `GdkMemoryFormat` enum value 67.
gtk-rs 0.9.6 predates the v4_24 feature and does not name this variant, but its
forward-compatible hidden enum fallback passes unknown numeric values through
unchanged. The probe uses that fallback only for the system-memory comparison.

Before importing either DMA-BUF, the probe maps it again for CPU readback and
prints the byte count, mismatch count against the source pattern, FNV-1a
checksum, and first pixel. This verifies that the GBM mapping contains the
bytes we intended to write.

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

If the PQ memory image and SDR DMA-BUF appear but XRGB2101010 remains black,
the failure is specific to the packed 10-bit opaque format or its import path.
The ARGB2101010 column distinguishes an XRGB padding/opaque-format issue from
a broader 10-bit import issue: if ARGB works while XRGB is black, focus on
XR30 handling; if both 10-bit formats are black while SDR works, focus on
GDK/Mesa's 10-bit DMA-BUF import or sampling support.

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

No production decoder, frame scheduler, subtitle, audio, or playback code is
changed by this probe.
