# Glimpse

A Linux video player written in Rust using GTK4, libadwaita, FFmpeg, and native PipeWire audio output.

- **Repository:** https://github.com/Mel34/Glimpse
- **Application ID:** `com.github.Mel34.Glimpse`
- **License:** LGPL-2.1-or-later

## Playback architecture

Glimpse uses its in-process `glimpse-engine` for playback; GTK `MediaFile` is no longer part of the application.

- FFmpeg/libavformat demuxes media and routes timestamped packets into bounded audio, video, and subtitle queues.
- FFmpeg/libavcodec decodes video in a worker thread. Decoded frames are presented as GTK memory textures and scheduled against the audio master clock.
- Audio is decoded and sent directly to PipeWire; video-only files use the monotonic playback clock.
- Seeking restarts the video demux/decode run at the target while the audio engine seeks its master timeline. Paused seeks remain paused.
- Audio track selection is supported; subtitle playback is not available yet.

## Rendering roadmap

The current video output path presents decoded frames through GTK memory
textures. The longer-term goal is a GPU-native rendering pipeline built around
[libplacebo](https://github.com/haasn/libplacebo), with Vulkan as the eventual
video output backend.

The current FFmpeg `swscale` conversion does not yet explicitly apply each
decoded frame's color-matrix and range metadata. This can affect color accuracy
for sources that rely on non-default conversion settings. Complete color
management—including transfer characteristics and HDR handling—is part of the
planned libplacebo rendering work below.

The work is expected to progress in stages:

1. **Complete hardware-frame interoperability.** Build on the VAAPI and DRM
   PRIME diagnostics to establish reliable frame export/import, format and
   modifier handling, synchronization, and resource lifetime across supported
   hardware and drivers.
2. **Integrate libplacebo.** Use libplacebo for GPU video processing and
   presentation, including correct color-space, range, transfer-function, and
   HDR handling. Preserve the existing FFmpeg decode and PipeWire audio
   pipeline while the rendering backend evolves.
3. **Introduce Vulkan video output.** Add a Vulkan-backed renderer using
   libplacebo, importing hardware-decoded frames where the platform supports
   it. Prefer direct GPU resource sharing to avoid unnecessary CPU readbacks
   and copies.
4. **Harden and retain fallbacks.** Validate synchronization, seeking,
   resizing, high-bit-depth formats, and sustained playback across different
   GPUs and drivers. Keep a compatible fallback path for systems where the
   preferred hardware or Vulkan interop is unavailable.

The VAAPI → DRM PRIME → EGL/OpenGL work under
`tools/diagnostics/vaapi-egl/` is exploratory validation, not the production
renderer. Its successful format-import and sample-preservation tests are
useful groundwork, but do not yet constitute libplacebo integration, Vulkan
output, complete color management, or a finished zero-copy playback path.
These milestones remain future work; the GTK texture renderer remains the
current presentation path.

## Interaction

1. Single click toggles playback; double click toggles fullscreen.
2. Dragging the video surface moves the window without toggling playback.
3. The seekbar wheel seeks in five-second steps.
4. The window wheel changes volume in five-percent steps.
5. Playback controls appear on pointer movement and hide after three seconds of inactivity in windowed and fullscreen modes.

## Build

Arch Linux prerequisites:

```bash
sudo pacman -S --needed ffmpeg gtk4 libadwaita cargo clang pkgconf alsa-lib pipewire
```

```bash
export BINDGEN_EXTRA_CLANG_ARGS="-I$PWD/include"
cargo check --workspace
cargo test --workspace --all-features
cargo run -p glimpse -- sample.mp4
```

Build the Arch package from the repository checkout with `makepkg -if`.
