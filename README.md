# Glimpse

A clean, predictable Linux video player written in Rust using GTK4, libadwaita, and an evolving in-process media engine. Inspired by GNOME Showtime, with the interaction design corrected according to the author's patches.

- **Repository:** [https://github.com/Mel34/Glimpse](https://github.com/Mel34/Glimpse)
- **Application ID:** `com.github.Mel34.Glimpse`
- **License:** LGPL-2.1-or-later

---

## Current Architecture & Implementation Reality

Glimpse 0.1.0 maintains a dual architecture to preserve the working desktop player while the custom playback engine is developed:

1. **Active Playback Path (GTK4 `MediaFile`):**
   - Powers visible playback, audio output, volume, timeline seeking, and fullscreen mode in `ui/src/window.rs`.
   - Remains the user-facing playback baseline throughout the engine migration.

2. **Custom Playback Engine (`glimpse-engine`):**
   - Uses the system `ffprobe` executable for real container duration and stream metadata.
   - The optional `ffmpeg-system` feature enables in-process FFmpeg 9 packet demuxing through `libavformat`.
   - Video, audio, and subtitle packets are routed into separate bounded queues with packet timestamps, stream time bases, keyframe flags, and serial tags.
   - The feature-gated software video decoder consumes queued packets with libavcodec, drains delayed frames, and converts decoded output to tightly packed RGBA with stream-timestamp-derived PTS.
  - Demuxing and decoding are blocking operations intended for dedicated worker threads. The diagnostic decode mode exercises the real demux-to-decoder queue path; GTK presentation and real audio output are not implemented yet. The mock audio sink remains scaffolding.

---

## Key Interaction Principles (Implemented)

1. **No Center Overlay Buttons:** Clean, uninterrupted video frame. Play/pause is accessed via the bottom control bar or directly on the video viewport.
2. **Video Click Arbitration:**
   - Single click toggles Play/Pause (delayed by GTK double-click time, default 250ms).
   - Double click toggles Fullscreen (cancels the pending play/pause timer immediately).
   - Window dragging moves the window freely without pausing playback.
3. **Seek Bar Wheel Seeking:** Mouse wheel over the seek bar seeks in exact 5-second steps (`wheel up = +5s`, `wheel down = -5s`). Consumes the event and does not adjust volume.
4. **Window Volume Wheel:** Mouse wheel anywhere else in the window adjusts volume in 5% steps (`0% ..= 100%`).
5. **Subtitles Off by Default:** Opening any file keeps subtitles strictly disabled until explicitly turned on by the user via the audio/subtitle track menu.

---

## Migration Roadmap (Implemented vs. Planned)

- **Milestone 0: Workspace & Toolchain** — `[Implemented]` Multi-crate Cargo workspace, LGPL-2.1 license compliance, Arch PKGBUILD, and desktop assets.
- **Milestone 1: Real Container Probe** — `[Implemented]` Actual container duration and stream discovery via system `ffprobe`.
- **Milestone 2: In-process Packet Demuxing** — `[Implemented: feature-gated]` FFmpeg 9/libavformat packet reading, per-stream-type bounded queues, timestamps, keyframe metadata, and serial tagging.
- **Milestone 3: Software Video Decode** — `[Implemented: feature-gated]` libavcodec software decoding, delayed-frame draining, timestamped frames, and RGBA conversion. The diagnostic CLI exercises the real-file demux/decode path; GTK presentation remains the next milestone.
- **Milestone 4: Presentation Surface Bridge** — `[Planned]` Connect decoded frames to GTK4 without breaking the working `MediaFile` baseline.
- **Milestone 5: Real Audio Decode and Output** — `[Planned]` Decode, resample, and output audio.
- **Milestone 6: A/V Sync and Seeking** — `[Planned]` Coordinate clocks, seek the demuxer, and flush decoder state.
- **Milestone 7: Hardware Acceleration** — `[Planned]` VA-API and DMA-BUF with software fallback.

---

## Building & Packaging

### Arch Linux Prerequisites

```bash
sudo pacman -S --needed ffmpeg gtk4 libadwaita cargo clang pkgconf alsa-lib libpulse libva-mesa-driver
```

### Build & Test

```bash
# Default workspace tests (does not link the optional native FFmpeg backend)
cargo test --workspace

# Build and test the in-process FFmpeg 9 demuxer and software decoder
cargo test --workspace --features glimpse-engine/ffmpeg-system

# Smoke-test real packet demuxing and software video decoding
cargo run -p glimpse-engine --features ffmpeg-system --bin glimpse_probe -- --decode sample.mp4

# Run diagnostic container probe CLI
cargo run --bin glimpse_probe -- sample.mp4

# Run desktop GTK4 player
cargo run --bin glimpse -- sample.mp4
```

### Build Arch Linux Package

```bash
makepkg -if
```
