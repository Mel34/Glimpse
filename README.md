# Glimpse

<img width="770" height="770" alt="Screenshot From 2026-11-11 22-55-54" src="https://github.com/user-attachments/assets/06813dfc-abd9-4e39-a6aa-4e02124916da" />

Glimpse is an independent **Showtime replacement for the GNOME desktop**. It aims to provide the same focused, native GTK4/libadwaita video-player experience, while building on a different playback engine and adding a few extra controls.

A Linux media player written in Rust with GTK4, libadwaita, an in-process FFmpeg playback engine, and native PipeWire audio output.

## Glimpse and GNOME Showtime

Glimpse is designed as a practical alternative to [GNOME Showtime (Video Player)](https://apps.gnome.org/Showtime/). The table below compares the user-facing playback features documented by GNOME with the features available in Glimpse 0.5.0.

| Feature | GNOME Showtime | Glimpse 0.5.0 |
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

Both applications share the same goal: a straightforward GNOME-native video player that gets out of the way while watching. Glimpse differentiates itself with its in-process FFmpeg engine, direct PipeWire audio output, A–B looping, and volume amplification up to 150%. Version 0.5.0 makes VAAPI hardware decoding the preferred default and adds GTK DMA-BUF presentation for supported NV12/P010 frames, graphics offload, more robust decoder/presentation fallbacks, and improved seek handling and diagnostics.


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
- FFmpeg/libavcodec prefers VAAPI hardware decoding by default when a compatible hardware decoder and device are available. Supported NV12 and P010 VAAPI frames are exported as DRM PRIME/DMA-BUF and imported as GTK DMA-BUF textures. The video surface is hosted through GTK's `GtkGraphicsOffload` integration where supported, allowing the compositor to handle the video surface separately from the regular GTK scene.
- If VAAPI initialization or initial packet submission is unsupported, Glimpse falls back to software decoding. If DMA-BUF presentation cannot be used, it disables further import attempts for that playback run and downloads frames for RGBA/GTK texture presentation. Set `GLIMPSE_VIDEO_DECODER=software` to force software decoding for diagnostics.
- DMA-BUF import and graphics offload reduce avoidable CPU-side conversion and compositing work, but do not guarantee end-to-end zero-copy or direct scanout. Those optimizations depend on the GPU, driver, compositor, format/modifier support, and the active overlays and transforms.
- Audio is decoded and written directly to PipeWire. The audio clock is authoritative when an audio stream is active; video-only playback uses a monotonic clock.
- Seeking coordinates the audio and video playback paths, seeks to a preceding video keyframe, and discards decoded frames before the requested presentation point. Paused seeks remain paused.
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

## Rendering architecture and roadmap

The current renderer supports two GTK texture paths:

- **Preferred hardware path:** VAAPI-decoded NV12/P010 frames are exported through DRM PRIME/DMA-BUF and imported as `GdkDmabufTexture` objects. The video surface is integrated with `GtkGraphicsOffload` on supported GTK/compositor combinations.
- **Compatibility fallback:** when DMA-BUF import is unavailable or fails, Glimpse downloads the VAAPI frame, converts it to RGBA, and presents it through a GTK memory texture. Software-decoded frames also use the RGBA/GTK texture path.

This is the production presentation architecture in 0.5.0; the VAAPI → DRM PRIME → EGL/OpenGL harness under `tools/diagnostics/vaapi-egl/` is a separate diagnostic tool. DMA-BUF import and graphics offload are not guarantees of end-to-end zero-copy or direct scanout: actual behavior depends on hardware, drivers, compositor support, formats and modifiers, and whether transforms or overlays require composition.

Future rendering work can build on this foundation:

1. **Broaden interoperability and lifecycle validation.** Exercise more GPUs, drivers, compositors, formats, modifiers, synchronization paths, and long-running playback scenarios.
2. **Integrate libplacebo.** Evaluate GPU video processing and presentation, including color management and HDR handling, while preserving the FFmpeg decode and PipeWire audio pipeline.
3. **Introduce Vulkan output if it provides a practical benefit.** Use libplacebo and hardware-frame import where platform support is reliable; avoid unnecessary CPU readbacks and copies.
4. **Keep compatibility paths.** Retain software decoding and RGBA presentation for systems where VAAPI or DMA-BUF interoperability is unavailable.

## Build

Arch Linux prerequisites:

```bash
sudo pacman -S --needed ffmpeg gtk4 libadwaita libdrm cargo rust clang pkgconf alsa-lib libpulse pipewire
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
