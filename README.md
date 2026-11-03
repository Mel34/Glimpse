# Glimpse

A clean, predictable Linux video player written in Rust using GTK4, libadwaita, and an in-process FFmpeg (`libav*`) media engine. Inspired by GNOME Showtime, with the interaction design corrected according to the author's patches.

- **Repository:** [https://github.com/Mel34/Glimpse](https://github.com/Mel34/Glimpse)
- **Application ID:** `com.github.Mel34.Glimpse`
- **License:** LGPL-2.1-or-later

---

## Key Interaction Principles

1. **No Center Overlay Buttons:** Clean, uninterrupted video frame. Play/pause is accessed via the bottom control bar or directly on the video viewport.
2. **Video Click Arbitration:**
   - Single click toggles Play/Pause (delayed by GTK double-click time, default 250ms).
   - Double click toggles Fullscreen (cancels the pending play/pause timer immediately).
   - Window dragging moves the window freely without pausing playback.
3. **Seek Bar Wheel Seeking:** Mouse wheel over the seek bar seeks in exact 5-second steps (`wheel up = +5s`, `wheel down = -5s`). Consumes the event and does not adjust volume.
4. **Window Volume Wheel:** Mouse wheel anywhere else in the window adjusts volume in 5% steps (`0% ..= 100%`).
5. **Subtitles Off by Default:** Opening any file keeps subtitles strictly disabled until explicitly turned on by the user via the audio/subtitle track menu.

---

## Architecture & Migration Roadmap

Glimpse currently operates on a dual-architecture baseline designed for clean migration:

1. **Active Playback Path (GTK4 `MediaFile`):**
   - High-performance native GTK media backend powering the UI window, video viewport, volume, seeking, and timeline controls.
2. **Custom Media Engine (`glimpse-engine`):**
   - Direct FFmpeg `libav*` pipeline in pure Rust without GStreamer or libmpv.
   - Demuxer thread with bounded packet/frame queues and serial invalidation on seeks.
   - Hardware audio master clock with soundcard latency deduction.
   - Presentation scheduler for zero-jitter frame delivery.

### 8-Milestone Implementation Plan

- **Milestone 0: Workspace & Toolchain** — Multi-crate Cargo workspace, LGPL-2.1 license compliance, Arch PKGBUILD.
- **Milestone 1: Container Probe CLI (`glimpse_probe`)** — FFmpeg `avformat` demuxing on separate thread, monotonic PTS validation.
- **Milestone 2: Audio Master Clock (`cpal`)** — Real-time audio rendering, latency deduction, and pause/resume precision.
- **Milestone 3: Presentation Surface Bridge** — Connecting engine's `VideoScheduler` to GTK4 via `gdk4::MemoryTexture` / `gtk4::CustomPaintable`.
- **Milestone 4: Interaction Engine** — State-machine click arbitration, 5s seek wheel, 5% window volume wheel, drag-move pass-through.
- **Milestone 5: Dynamic Tracks & Subtitles** — Real container stream switching and subtitle overlay rendering.
- **Milestone 6: Linux Desktop Polish** — MPRIS D-Bus integration, XDG desktop actions, and drag-and-drop file ingestion.
- **Milestone 7: Hardware Acceleration (VA-API)** — Zero-copy DMA-BUF textures (`GdkDmabufTextureBuilder`) on AMD Radeon (Mesa) and Intel with automatic software fallback.

---

## Building & Packaging

### Arch Linux Prerequisites

```bash
sudo pacman -S --needed ffmpeg gtk4 libadwaita cargo clang pkgconf alsa-lib libpulse libva-mesa-driver
```

### Build & Test

```bash
# Test pure logic (clock, queues, seek clamping)
cargo test --workspace

# Run diagnostic container probe CLI
cargo run --bin glimpse_probe -- sample.mp4
cargo run --bin glimpse_probe -- --play sample.mp4

# Run desktop GTK4 player
cargo run --bin glimpse -- sample.mp4
```

### Build Arch Linux Package

```bash
makepkg -if
```