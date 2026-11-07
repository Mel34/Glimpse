# Glimpse

A clean, predictable Linux video player written in Rust using GTK4, libadwaita, and an evolving in-process media engine. Inspired by GNOME Showtime, with the interaction design corrected according to the author's patches.

- **Repository:** [https://github.com/Mel34/Glimpse](https://github.com/Mel34/Glimpse)
- **Application ID:** `com.github.Mel34.Glimpse`
- **License:** LGPL-2.1-or-later

---

## Current Architecture & Implementation Reality

Glimpse 0.1.0 maintains a dual architecture to guarantee a rock-solid user experience while architecting a custom playback engine from first principles:

1. **Active Playback Path (GTK4 `MediaFile`):**
   - Powers all visible playback, container demuxing, video presentation to GTK `Picture`, audio output, volume, timeline seeking, and fullscreen mode in `ui/src/window.rs`.
   - Ensures an immediate, working, hardware-accelerated media player on modern Linux desktops.

2. **Custom Playback Engine Scaffold (`glimpse-engine`):**
   - Architectural foundation for an in-process media pipeline in pure Rust.
   - Contains clean abstractions and algorithms: audio clock drift math, latency deduction, serial queue invalidation, and frame presentation scheduling.
   - Demuxer and decoder currently return mock/synthetic frames and tracks while the presentation bridge to GTK4 is being designed.
   - Native `ffmpeg-the-third` and `cpal` backends remain optional; the FFmpeg binding is upgraded to the 6.x line, which supports system FFmpeg 5.1 through 9.0.

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

- **Milestone 0: Workspace & Toolchain** — `[Implemented]` Multi-crate Cargo workspace, LGPL-2.1 license compliance, Arch PKGBUILD, inverted SVG assets, AppStream metadata.
- **Milestone 1: Container Probe CLI (`glimpse_probe`)** — `[Scaffold]` Stream discovery CLI structure and monotonic PTS loop verification. Real `libavformat` integration in progress.
- **Milestone 2: Audio Master Clock (`cpal`)** — `[Scaffold]` Master clock algorithm, latency offset math, and unit tests implemented. Real soundcard hardware sink planned.
- **Milestone 3: Presentation Surface Bridge** — `[Planned]` Connecting engine's frame scheduler to GTK4 via `gdk4::MemoryTexture` / `gtk4::CustomPaintable` without breaking the working `MediaFile` baseline.
- **Milestone 4: Interaction Engine** — `[Implemented]` State-machine click arbitration, 5s seekbar wheel, 5% window volume wheel, drag-move pass-through.
- **Milestone 5: Dynamic Tracks & Subtitles** — `[Implemented in UI / Planned in Engine]` Track selector popover in GTK; engine container stream switching planned.
- **Milestone 6: Linux Desktop Polish** — `[Implemented]` Drag-and-drop file ingestion, MIME type associations, desktop action shortcuts. MPRIS integration planned.
- **Milestone 7: Hardware Acceleration (VA-API)** — `[Planned]` Zero-copy DMA-BUF textures (`GdkDmabufTextureBuilder`) for AMD Radeon (Mesa) and Intel with automatic software fallback.

---

## Building & Packaging

### Requirements

- Rust 1.83 or newer.
- GTK 4.14 or newer (the Rust bindings expose the GDK DMA-BUF texture builder API from GTK 4.14).
- libadwaita 1.5 or newer.

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