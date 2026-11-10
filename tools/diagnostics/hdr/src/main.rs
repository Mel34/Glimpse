//! Static BT.2020/PQ test pattern backed by a GBM DMA-BUF.
//!
//! This isolates GDK's HDR color-state and DMA-BUF import paths from FFmpeg
//! and the playback scheduler. The buffer is 10-bit XRGB in a GBM allocation;
//! it is not a decoded video frame and does not test YUV/P010 import.

use gdk4::{ColorState, DmabufTextureBuilder};
use gbm::{BufferObjectFlags, Device, Format, Modifier};
use gtk4::prelude::*;
use gtk4::{
    Application, ApplicationWindow, Box as GtkBox, GraphicsOffload,
    GraphicsOffloadEnabled, Label, Orientation, Picture,
};
use std::error::Error;
use std::fmt::Display;
use std::fs::OpenOptions;
use std::io;
use std::os::fd::AsRawFd;

const WIDTH: u32 = 1920;
const HEIGHT: u32 = 1080;
const LEVELS_NITS: [f64; 14] = [
    0.1, 1.0, 5.0, 10.0, 25.0, 50.0, 75.0,
    100.0, 203.0, 400.0, 600.0, 1000.0, 2000.0, 4000.0,
];
const DRM_FORMAT_MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;

fn stage<T, E: Display>(result: Result<T, E>, name: &str) -> Result<T, Box<dyn Error>> {
    result.map_err(|error| io::Error::other(format!("{name}: {error}")).into())
}

/// SMPTE ST 2084 (PQ) OETF: absolute display luminance in cd/m² to [0, 1].
fn pq_encode(nits: f64) -> u16 {
    let m1 = 2610.0 / 16384.0;
    let m2 = 2523.0 / 32.0;
    let c1 = 3424.0 / 4096.0;
    let c2 = 2413.0 / 128.0;
    let c3 = 2392.0 / 128.0;

    let luminance = (nits.clamp(0.0, 10_000.0) / 10_000.0).powf(m1);
    let encoded = ((c1 + c2 * luminance) / (1.0 + c3 * luminance)).powf(m2);
    (encoded * u16::MAX as f64).round() as u16
}

fn modifier_value(modifier: Modifier) -> Result<u64, Box<dyn Error>> {
    match modifier {
        Modifier::Linear => Ok(0),
        Modifier::Invalid => Ok(DRM_FORMAT_MOD_INVALID),
        Modifier::Unrecognized(value) => Ok(value),
        other => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!("GBM returned an unsupported named modifier: {other:?}"),
        )
        .into()),
    }
}

fn build_pq_dmabuf_pattern(
    display: &gdk4::Display,
) -> Result<(gdk4::Texture, String), Box<dyn Error>> {
    let device_path = std::env::var("GLIMPSE_DRM_DEVICE")
        .unwrap_or_else(|_| "/dev/dri/renderD128".to_string());
    let drm_file = stage(
        OpenOptions::new()
            .read(true)
            .write(true)
            .open(&device_path),
        &format!("open DRM device {device_path}"),
    )?;
    let gbm = stage(Device::new(drm_file), "create GBM device")?;

    // XRGB2101010 preserves 10-bit PQ code values while keeping this first
    // DMA-BUF probe to one packed plane.
    let format = Format::Xrgb2101010;
    // The pattern is CPU-written, so request an explicitly linear layout.
    // WRITE alone can be accepted by the capability query but rejected by
    // Mesa's allocator when it chooses a tiled layout.
    let usage = BufferObjectFlags::WRITE | BufferObjectFlags::LINEAR;
    if !gbm.is_format_supported(format, usage) {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!("GBM does not support linear XRGB2101010 WRITE on {device_path}"),
        )
        .into());
    }

    let mut bo = stage(
        gbm.create_buffer_object::<()>(WIDTH, HEIGHT, format, usage),
        "allocate linear XRGB2101010 GBM buffer",
    )?;
    let stride = bo.stride() as usize;
    let mut pixels = vec![0u8; stride * HEIGHT as usize];

    for y in 0..HEIGHT as usize {
        let row = &mut pixels[y * stride..(y + 1) * stride];
        for x in 0..WIDTH as usize {
            let band = (x * LEVELS_NITS.len() / WIDTH as usize)
                .min(LEVELS_NITS.len() - 1);
            let code16 = pq_encode(LEVELS_NITS[band]) as u32;
            let code10 = (code16 * 1023 + 32_767) / 65_535;
            // DRM_FORMAT_XRGB2101010: X[31:30], R[29:20], G[19:10], B[9:0].
            let packed = (3u32 << 30) | (code10 << 20) | (code10 << 10) | code10;
            row[x * 4..x * 4 + 4].copy_from_slice(&packed.to_ne_bytes());
        }
    }
    stage(bo.write(&pixels), "write test pattern into GBM buffer")?;

    if bo.plane_count() != 1 {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!("Expected one DMA-BUF plane, GBM allocated {}", bo.plane_count()),
        )
        .into());
    }

    let modifier = modifier_value(bo.modifier())?;
    let dmabuf = stage(bo.fd(), "export GBM buffer as DMA-BUF fd")?;
    let raw_fd = dmabuf.as_raw_fd();

    let builder = DmabufTextureBuilder::new();
    builder.set_display(display);
    builder.set_width(WIDTH);
    builder.set_height(HEIGHT);
    builder.set_fourcc(format as u32);
    builder.set_modifier(modifier);
    builder.set_n_planes(1);
    builder.set_fd(0, raw_fd);
    builder.set_offset(0, bo.offset(0));
    builder.set_stride(0, bo.stride());
    builder.set_color_state(Some(&ColorState::rec2100_pq()));

    // GDK retains the DMA-BUF after build. Keep its exported fd alive until
    // the texture is finalized, rather than closing it when this function exits.
    let texture = stage(unsafe { builder.build_with_release_func(move || drop(dmabuf)) }, "import DMA-BUF into GDK texture")?;

    let details = format!(
        "GBM DMA-BUF · XRGB2101010 · {}×{} · stride {} · modifier 0x{modifier:016x}",
        WIDTH,
        HEIGHT,
        bo.stride()
    );
    println!("{details}");
    Ok((texture, details))
}

fn main() -> glib::ExitCode {
    let app = Application::builder()
        .application_id("com.github.Mel34.Glimpse.HDRProbe")
        .build();

    app.connect_activate(|app| {
        let window = ApplicationWindow::builder()
            .application(app)
            .title("Glimpse — HDR / DMA-BUF probe")
            .default_width(1200)
            .default_height(760)
            .build();

        let picture = Picture::new();
        picture.set_content_fit(gtk4::ContentFit::Contain);
        picture.set_hexpand(true);
        picture.set_vexpand(true);

        let (details, status) = match build_pq_dmabuf_pattern(&gtk4::prelude::WidgetExt::display(&window)) {
            Ok((texture, details)) => {
                picture.set_paintable(Some(&texture));
                (details, "DMA-BUF texture imported successfully".to_string())
            }
            Err(error) => {
                let message = format!("DMA-BUF probe unavailable: {error}");
                eprintln!("{message}");
                (message.clone(), message)
            }
        };

        let offload = GraphicsOffload::new(Some(&picture));
        offload.set_enabled(GraphicsOffloadEnabled::Enabled);
        offload.set_hexpand(true);
        offload.set_vexpand(true);

        let levels = LEVELS_NITS
            .iter()
            .map(|level| format!("{level}"))
            .collect::<Vec<_>>()
            .join("  ·  ");
        let explanation = Label::new(Some(&format!(
            "{status}\n\
             BT.2020 / ST 2084 PQ · left to right: {levels} cd/m²\n\
             {details}\n\
             10-bit packed RGB DMA-BUF · GTK graphics offload requested\n\
             This tests RGB DMA-BUF import, not decoder YUV/P010 import."
        )));
        explanation.set_wrap(true);
        explanation.set_selectable(true);

        let content = GtkBox::new(Orientation::Vertical, 12);
        content.set_margin_top(12);
        content.set_margin_bottom(12);
        content.set_margin_start(12);
        content.set_margin_end(12);
        content.append(&offload);
        content.append(&explanation);
        window.set_child(Some(&content));
        window.present();
    });

    app.run()
}

#[cfg(test)]
mod tests {
    use super::pq_encode;

    #[test]
    fn pq_reference_codes_are_close_to_st2084_values() {
        assert!((pq_encode(100.0) as i32 - 33_297).abs() <= 1);
        assert!((pq_encode(203.0) as i32 - 38_055).abs() <= 1);
        assert_eq!(pq_encode(10_000.0), u16::MAX);
    }

    #[test]
    fn ten_bit_quantization_preserves_reference_white() {
        let code16 = pq_encode(203.0) as u32;
        let code10 = (code16 * 1023 + 32_767) / 65_535;
        assert!((code10 as i32 - 594).abs() <= 1);
    }
}
