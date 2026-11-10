# GTK HDR / DMA-BUF diagnostic

This probe is a standalone Cargo workspace, deliberately isolated from
Glimpse's production workspace. It uses GTK 4.24-era Rust bindings
(gtk4/gdk4 0.9), while the production application remains on its existing
GTK 4.12 binding baseline (gtk4/gdk4 0.8) with libadwaita 0.6.

The isolation is required because Cargo permits only one crate in a dependency
graph to link the native `gtk-4` library. The production app and this probe
therefore have separate dependency graphs and lockfiles.

## What it displays

The window has five columns:

1. **PQ XRGB DMA-BUF** — a GBM-allocated, linear XRGB2101010 buffer tagged
   with Rec. 2100 PQ.
2. **PQ system memory** — a GDK memory texture made from the exact same
   XRGB2101010 bytes and the same PQ color state.
3. **PQ ARGB DMA-BUF** — the same 10-bit PQ ramp in ARGB2101010, with explicit
   opaque alpha rather than unused X/padding bits.
4. **P010 DMA-BUF** — a two-plane P010 image tagged with Rec. 2100 PQ. The Y
   plane contains 14 limited-range PQ luminance bands; the interleaved UV plane
   is neutral (U=V=512), deliberately avoiding dependence on YCbCr matrix
   selection in this first import test.
5. **SDR DMA-BUF control** — a separate GBM-allocated XRGB8888 buffer using
   GDK's default sRGB color state.

The P010 image uses separate linear GBM allocations for its Y and UV memory
planes, passed to GDK as two DMA-BUF file descriptors. This exercises GDK's
multi-plane P010 import and sampling path without assuming how GBM exposes a
multi-planar BO through its CPU mapping API. It is a valid multi-fd plane
layout, but it is not yet a reproduction of the single-fd/multi-offset layout
often returned by VAAPI decoders.

P010 stores 10-bit samples in the most-significant bits of 16-bit words. The
luma values are quantized to the limited video range [64, 940]; neutral chroma
uses 512 for both components. The pattern is grayscale so that the first test
isolates P010 plane import and PQ transfer handling from chroma matrix issues.

The first two columns isolate the RGB DMA-BUF path from pixel packing, PQ
encoding, and color-state differences. The ARGB column retains the known-good
10-bit packed RGB route as a reference, while XRGB8888 remains the ordinary
8-bit DMA-BUF control.

GTK 4.24 added `GDK_MEMORY_XRGB2101010` as `GdkMemoryFormat` enum value 67.
gtk-rs 0.9.6 predates the v4_24 feature and does not name this variant, but its
forward-compatible hidden enum fallback passes unknown numeric values through
unchanged. The probe uses that fallback only for the system-memory comparison.

Before importing each DMA-BUF, the probe maps its GBM allocation again for CPU
readback and prints the byte count, mismatch count against the source pattern,
FNV-1a checksum, and first bytes. This verifies the contents of each plane
before GDK imports it.

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
If ARGB2101010 works, that gives us a known-good packed RGB HDR reference.
The P010 column then independently tells us whether GTK/Mesa can import and
sample a multi-plane 10-bit YUV DMA-BUF with PQ color state.

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
