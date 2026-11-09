# Glimpse

A Linux video player written in Rust using GTK4, libadwaita, FFmpeg, and native PipeWire audio output.

- **Repository:** https://github.com/Mel34/Glimpse
- **Application ID:** `com.github.Mel34.Glimpse`
- **License:** LGPL-2.1-or-later

## Playback architecture

Glimpse uses its in-process `glimpse-engine` for playback; GTK `MediaFile` is no longer part of the application.

- FFmpeg/libavformat demuxes media and routes timestamped packets into bounded audio, video, and subtitle queues.
- FFmpeg/libavcodec decodes video in a worker thread. Decoded frames are presented as GTK memory textures and scheduled against the audio master clock.
- Audio is decoded and sent directly to PipeWire.
- Seeking restarts the video demux/decode run at the target while the audio engine seeks its master timeline. Paused seeks remain paused.
- Track selection is exposed through the GTK controls; subtitles remain off by default.

## Interaction

1. Single click toggles playback; double click toggles fullscreen.
2. Dragging the video surface moves the window without toggling playback.
3. The seekbar wheel seeks in five-second steps.
4. The window wheel changes volume in five-percent steps.
5. Fullscreen controls appear on pointer movement and hide after two seconds of inactivity.

## Build

Arch Linux prerequisites:

```bash
sudo pacman -S --needed ffmpeg gtk4 libadwaita cargo clang pkgconf alsa-lib pipewire
```

```bash
cargo check --workspace
cargo test --workspace --all-features
cargo run -p glimpse -- sample.mp4
```

Build the Arch package from the repository checkout with `makepkg -if`.
