# Glimpse

Glimpse is an independent **Showtime replacement for the GNOME desktop**. It aims to provide the same focused, native GTK4/libadwaita video-player experience, while building on a different playback engine and adding a few extra controls.

A Linux media player written in Rust with GTK4, libadwaita, an in-process FFmpeg playback engine, and native PipeWire audio output.

## Glimpse and GNOME Showtime

Glimpse is designed as a practical alternative to [GNOME Showtime (Video Player)](https://apps.gnome.org/Showtime/). The table below compares the user-facing playback features documented by GNOME with the features available in Glimpse 0.4.1.

| Feature | GNOME Showtime | Glimpse 0.4.1 |
| --- | --- | --- |
| Distraction-free playback controls | Yes | Yes |
| Fullscreen playback | Yes | Yes |
| Adjustable playback speed | Yes | Yes — continuous 0.5×–2.0× |
| Multiple audio tracks | Yes | Yes |
| Multiple subtitle tracks | Yes | Yes |
| Video rotation | Yes | Yes — 90° clockwise or counterclockwise |
| Screenshot capture | Yes | Yes — PNG saved to `~/Pictures/Screenshots/` |
| Volume amplification above 100% | Not listed in GNOME's feature overview | Yes — up to 150% |
| Whole-file looping | Not listed in GNOME's feature overview | Yes |
| A–B section looping | Not listed in GNOME's feature overview | Yes |
| External subtitle format details | Not specified in GNOME's feature overview | SRT, WebVTT, ASS, and SSA |

The comparison focuses on documented player features rather than implementation quality or codec benchmarks. “Not listed” means the feature is not advertised in GNOME's official feature overview; it does not claim that Showtime cannot support it.

Both applications share the same goal: a straightforward GNOME-native video player that gets out of the way while watching. Glimpse differentiates itself with its in-process FFmpeg engine, direct PipeWire audio output, A–B looping, and volume amplification up to 150%. Version 0.4.1 polishes the playback menus and controls, improves shortcut discoverability, and adds dedicated keyboard shortcuts for screenshots and playback speed.


- **Repository:** https://github.com/Mel34/Glimpse
- **Application ID:** `com.github.Mel34.Glimpse`
- **License:** LGPL-2.1-or-later

## Features

- **Native playback engine.** Demuxing and decoding are handled in-process through FFmpeg's `libav*` libraries; playback does not rely on GStreamer, GTK `MediaFile`, or libmpv.
- **Native PipeWire audio.** Decoded audio is sent directly to PipeWire, which provides the audio master clock for playback.
- **Audio track selection.** Switch between embedded audio streams or turn audio off.
- **Subtitle support.** Select embedded text subtitles or load an external `.srt`, `.ass`, `.ssa`, or `.vtt` file. Subtitles are off by default. Available embedded tracks are ordered using the system language preferences, with English as a fallback preference. Bitmap subtitle formats such as PGS and DVD subtitles are not rendered.
- **Synchronized video playback.** Video frames are scheduled against the audio clock; video-only media uses a monotonic playback clock.
- **Playback speed.** Adjust playback continuously from 0.5× to 2.0× in the playback menu, or use the keyboard shortcuts to change speed in 0.1× steps, halve or double it, and reset to 1×.
- **Video rotation.** Rotate the picture 90° clockwise or counterclockwise from the playback menu. Rotation is applied to the displayed video and captured screenshots.
- **Screenshots.** Capture the current video frame without the playback controls. PNG screenshots are saved automatically to `~/Pictures/Screenshots/`, using unique, timestamped filenames.
- **Volume control.** Adjust or mute audio, with software volume amplification up to 150%.
- **Playback controls.** Seek, pause and resume, and toggle fullscreen. Controls automatically hide after inactivity.
- **Looping.** Loop the entire file or define an A–B section.
- **Flexible input.** Open media through the file chooser, drag and drop a file, or launch Glimpse with a media path or URI. Multiple windows are supported.
- **GTK4 and libadwaita interface.** Includes persistent System, Light, and Dark appearance preferences.

## Playback architecture

Glimpse uses its in-process `glimpse-engine` for playback.

- FFmpeg/libavformat demuxes media and routes timestamped packets into bounded audio, video, and subtitle queues.
- FFmpeg/libavcodec decodes media. VAAPI hardware decoding is preferred by default, with supported NV12/P010 frames imported through GTK DMA-BUF textures. If VAAPI setup or initial packet submission is unsupported, Glimpse falls back to software decoding and the RGBA/GTK texture path. Set `GLIMPSE_VIDEO_DECODER=software` to force software decoding for diagnostics. Video decoding runs independently from the GTK interface, and decoded frames are handed to the presentation path.
- Audio is decoded and written directly to PipeWire. The audio clock is authoritative when an audio stream is active; video-only playback uses a monotonic clock.
- Seeking coordinates the audio and video playback paths. Paused seeks remain paused.
- Embedded and external text subtitles are rendered over the video. Subtitle selection is independent of audio-track selection.

## Interaction and keyboard shortcuts

| Action | Control |
| --- | --- |
| Play / pause | `Space` or single-click the video |
| Toggle fullscreen | `F` or double-click the video |
| Leave fullscreen | `Esc` |
| Seek backward / forward | `←` / `→` (5 seconds) |
| Adjust volume | `↑` / `↓` (5%, up to 150%) |
| Mute / unmute | `M` |
| Capture screenshot | `s` |
| Decrease / increase playback speed | `[` / `]` (0.1× steps) |
| Halve / double playback speed | `{` / `}` |
| Reset playback speed | `Backspace` |
| Set A–B loop point A, then B; press again to clear | `L` |
| Toggle whole-file looping | `Shift+L` |
| Open media | `Ctrl+O` |
| New window | `Ctrl+N` |

Additional pointer controls:

- Scroll over the seekbar to seek in five-second steps. Scroll down to advance and up to rewind; seeking stops at the media boundaries.
- Scroll over the window to adjust volume in five-percent steps, up to 150%.
- Drag the video surface to move the window when it is not fullscreen.
- Playback speed and rotation are available from the playback menu; the screenshot button is beside the playback controls.
- Playback controls appear when the pointer moves and hide after three seconds of inactivity. Open popovers remain available while in use.

## Rendering roadmap

The current video presentation path uses GTK textures. The longer-term goal is a GPU-native rendering pipeline built around [libplacebo](https://github.com/haasn/libplacebo), with Vulkan as the eventual video output backend.

The work is expected to progress in stages:

1. **Complete hardware-frame interoperability.** Build on the VAAPI and DRM PRIME diagnostics to establish reliable frame export/import, format and modifier handling, synchronization, and resource lifetime across supported hardware and drivers.
2. **Integrate libplacebo.** Use libplacebo for GPU video processing and presentation, including color-space, range, transfer-function, and HDR handling. Preserve the existing FFmpeg decode and PipeWire audio pipeline while the rendering backend evolves.
3. **Introduce Vulkan video output.** Add a Vulkan-backed renderer using libplacebo, importing hardware-decoded frames where the platform supports it. Prefer direct GPU resource sharing to avoid unnecessary CPU readbacks and copies.
4. **Harden and retain fallbacks.** Validate synchronization, seeking, resizing, high-bit-depth formats, and sustained playback across different GPUs and drivers. Keep a compatible fallback path for systems where the preferred hardware or Vulkan interop is unavailable.

The VAAPI → DRM PRIME → EGL/OpenGL work under `tools/diagnostics/vaapi-egl/` is exploratory validation, not the production renderer. Its successful format-import and sample-preservation tests are useful groundwork, but do not yet constitute libplacebo integration, Vulkan output, or a universally available zero-copy playback path. The GTK texture presentation path remains the current renderer.

## Build

Arch Linux prerequisites:

```bash
sudo pacman -S --needed ffmpeg gtk4 libadwaita cargo rust clang pkgconf alsa-lib libpulse pipewire
```

Build and test:

```bash
export BINDGEN_EXTRA_CLANG_ARGS="-I$PWD/include"
cargo check --workspace
cargo test --workspace --all-features
cargo run -p glimpse -- sample.mp4
```

Build the Arch package from the repository checkout:

```bash
makepkg -if
```

For hardware-decoding and rendering experiments, see [the VAAPI/EGL diagnostic README](tools/diagnostics/vaapi-egl/README.md).
