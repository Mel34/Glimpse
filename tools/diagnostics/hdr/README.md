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

Run from the repository root, in the HDR-enabled Wayland session. GTK 4.24
provides GDK debug switches for color management and HDR rendering. The
earlier probe run enabled only offload and DMA-BUF diagnostics, so its lack of
surface image-description requests did **not** establish that GDK's HDR path
was inactive or broken.

First test the normal color-managed path:

```sh
GDK_DEBUG=color-mgmt,offload,dmabuf \
  cargo run --manifest-path tools/diagnostics/hdr/Cargo.toml
```

Then close the probe and run again with GTK's HDR-rendering debug override:

```sh
GDK_DEBUG=hdr,color-mgmt,offload,dmabuf \
  cargo run --manifest-path tools/diagnostics/hdr/Cargo.toml
```

The `hdr` switch forces HDR rendering; it is a diagnostic override, not a
production setting. Compare the visual result of both runs.

## Wayland protocol tracing

Capture each run separately so we can compare the normal and forced-HDR paths:

```sh
GDK_DEBUG=color-mgmt,offload,dmabuf WAYLAND_DEBUG=1 \
  cargo run --manifest-path tools/diagnostics/hdr/Cargo.toml \
  2>hdr-probe-normal.log

GDK_DEBUG=hdr,color-mgmt,offload,dmabuf WAYLAND_DEBUG=1 \
  cargo run --manifest-path tools/diagnostics/hdr/Cargo.toml \
  2>hdr-probe-forced-hdr.log
```

Then extract the relevant protocol and renderer diagnostics:

```sh
grep -Ei 'wp_color|image_description|set_image_description|offload|dmabuf|HDR|color management' \
  hdr-probe-normal.log hdr-probe-forced-hdr.log
```

Look specifically for a surface color-management object, creation of a
parametric image description, a ready event, and
`set_image_description` on the probe's surface. Color-manager capability
queries by Mesa on unrelated connections are not sufficient evidence.

Run windowed first, then fullscreen if the normal-path trace shows successful
color-description negotiation. Compare the bands with an HDR reference in
mpv. The 203 cd/m² patch is the reference-white checkpoint; higher patches
should remain distinguishable up to the display's capabilities.

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
