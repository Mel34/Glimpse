# GTK HDR / PQ diagnostic

This probe is a standalone Cargo workspace, deliberately isolated from
Glimpse's production workspace. It uses GTK 4.16-era Rust bindings
(gtk4/gdk4 0.9), while the production application remains on its existing
GTK 4.12 binding baseline (gtk4/gdk4 0.8) with libadwaita 0.6.

The isolation is required because Cargo permits only one crate in a dependency
graph to link the native `gtk-4` library. The production app and this probe
therefore have separate dependency graphs and lockfiles.

The probe creates an opaque, 16-bit RGB test pattern tagged with the GDK
Rec. 2100 PQ color state and places it in a `GtkGraphicsOffload` widget. The
bands are absolute PQ luminance values from 0.1 to 4000 cd/m².

## Run

From the repository root:

```sh
GDK_DEBUG=offload,dmabuf cargo run --manifest-path tools/diagnostics/hdr/Cargo.toml
```

For Wayland protocol tracing:

```sh
GDK_DEBUG=offload,dmabuf WAYLAND_DEBUG=1 \
  cargo run --manifest-path tools/diagnostics/hdr/Cargo.toml 2>hdr-probe-wayland.log
grep -Ei 'wp_color|image_description|set_image_description|offload|dmabuf' \
  hdr-probe-wayland.log
```

Run this in the HDR-enabled Wayland session, first windowed and then fullscreen.
Compare the bands with an HDR reference in mpv. The 203 cd/m² patch is the
reference-white checkpoint; higher patches should remain distinguishable up to
the display's capabilities.

## Scope and interpretation

This first probe tests GDK's PQ texture color-state path. It uses a system-memory
texture, not a DMA-BUF. GTK may therefore fall back from graphics offload to
normal GSK rendering. The requested offload state is not proof that offload
became active; inspect the `GDK_DEBUG` output.

A successful-looking pattern alone does not prove that GTK attached a PQ image
description to the Wayland surface. The protocol trace must show the surface
image-description negotiation, and the compositor's preferred description
must be checked. The next step, if this path is viable, is an opaque DMA-BUF
test texture so `GtkGraphicsOffload` can exercise the actual video path.

No production decoder, frame scheduler, subtitle, audio, or playback code is
changed by this probe.
