# Glimpse

A Linux video player written in Rust that uses FFmpeg's `libav*` libraries directly (no GStreamer, no libmpv), with a GTK4 + libadwaita UI. Inspired by GNOME Showtime, with the interaction design corrected according to author's patches.

## Key Interaction Principles

1. **No Center Overlay Buttons:** Clean, uninterrupted video frame.
2. **Video Click Arbitration:** Single click = play/pause (250ms timer); Double click = fullscreen.
3. **Seek Bar Wheel Seeking:** Mouse wheel over seek bar seeks in exact 5s steps.
4. **Window Volume Wheel:** Mouse wheel anywhere else adjusts volume in 5% steps.
5. **Subtitles Off by Default:** Media loads with subtitles strictly disabled until explicitly enabled.

## Building on Arch Linux

```bash
# Install prerequisites
sudo pacman -S --needed ffmpeg gtk4 libadwaita cargo clang pkgconf alsa-lib libva-mesa-driver

# Test pure logic (clock, queues, seek clamping)
cargo test --workspace

# Run diagnostic probe CLI
cargo run --bin glimpse_probe -- sample.mp4
cargo run --bin glimpse_probe -- --play sample.mp4

# Run desktop GTK4 player
cargo run --bin glimpse -- sample.mp4
```# Glimpse
