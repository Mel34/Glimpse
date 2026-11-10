//! Static BT.2020/PQ test pattern backed by a GBM DMA-BUF and a
//! byte-identical system-memory reference texture.

use gdk4::{ColorState, DmabufTextureBuilder, MemoryFormat, MemoryTextureBuilder};
use gbm::{BufferObjectFlags, Device, Format, Modifier};
use gtk4::prelude::*;
use gtk4::{Application, ApplicationWindow, Box as GtkBox, Label, Orientation, Picture};
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

/// SMPTE ST 2084 (PQ): absolute display luminance in cd/m² to [0, 1].
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

fn fill_pq_pattern(buffer: &mut [u8], stride: usize) {
    for y in 0..HEIGHT as usize {
        let row = &mut buffer[y * stride..(y + 1) * stride];
        for x in 0..WIDTH as usize {
            let band = (x * LEVELS_NITS.len() / WIDTH as usize)
                .min(LEVELS_NITS.len() - 1);
            let code16 = pq_encode(LEVELS_NITS[band]) as u32;
            let code10 = (code16 * 1023 + 32_767) / 65_535;
            // DRM_FORMAT_XRGB2101010 / GDK_MEMORY_XRGB2101010:
            // X[31:30], R[29:20], G[19:10], B[9:0].
            let packed = (3u32 << 30) | (code10 << 20) | (code10 << 10) | code10;
            row[x * 4..x * 4 + 4].copy_from_slice(&packed.to_ne_bytes());
        }
    }
}

fn build_pq_textures(
    display: &gdk4::Display,
) -> Result<(gdk4::Texture, gdk4::Texture, String), Box<dyn Error>> {
    let device_path = std::env::var("GLIMPSE_DRM_DEVICE")
        .unwrap_or_else(|_| "/dev/dri/renderD128".to_string());
    let drm_file = stage(
        OpenOptions::new().read(true).write(true).open(&device_path),
        &format!("open DRM device {device_path}"),
    )?;
    let gbm = stage(Device::new(drm_file), "create GBM device")?;

    let format = Format::Xrgb2101010;
    let usage = BufferObjectFlags::RENDERING | BufferObjectFlags::LINEAR;
    if !gbm.is_format_supported(format, usage) {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!("GBM does not support linear renderable XRGB2101010 on {device_path}"),
        )
        .into());
    }

    let mut bo = stage(
        gbm.create_buffer_object::<()>(WIDTH, HEIGHT, format, usage),
        "allocate linear renderable XRGB2101010 GBM buffer",
    )?;
    let stride = bo.stride() as usize;
    let mut cpu_pixels = Vec::new();
    stage(
        bo.map_mut(0, 0, WIDTH, HEIGHT, |mapping| {
            let mapped_stride = mapping.stride() as usize;
            cpu_pixels.resize(mapped_stride * HEIGHT as usize, 0);
            fill_pq_pattern(&mut cpu_pixels, mapped_stride);
            mapping.buffer_mut()[..cpu_pixels.len()].copy_from_slice(&cpu_pixels);
        }),
        "map and write test pattern into GBM buffer",
    )?;

    if bo.plane_count() != 1 {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!("Expected one DMA-BUF plane, GBM allocated {}", bo.plane_count()),
        )
        .into());
    }

    // This reference uses the exact same packed 10-bit bytes and PQ color
    // state as the DMA-BUF. If it renders while the left image is black,
    // pattern generation/color-state are sound and the fault is in DMA-BUF
    // sampling or its import metadata.
    let bytes = glib::Bytes::from_owned(cpu_pixels);
    let cpu_texture = MemoryTextureBuilder::new()
        .set_width(WIDTH as i32)
        .set_height(HEIGHT as i32)
        .set_stride(stride)
        .set_format(MemoryFormat::Xrgb2101010)
        .set_bytes(Some(&bytes))
        .set_color_state(&ColorState::rec2100_pq())
        .build();

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

    // Keep the exported fd alive until GDK finalizes the imported texture.
    let dmabuf_texture = stage(
        unsafe { builder.build_with_release_func(move || drop(dmabuf)) },
        "import DMA-BUF into GDK texture",
    )?;

    let first = u32::from_ne_bytes(bytes.as_ref()[0..4].try_into()?);
    let last_offset = (WIDTH as usize - 1) * 4;
    let last = u32::from_ne_bytes(bytes.as_ref()[last_offset..last_offset + 4].try_into()?);
    let details = format!(
        "GBM DMA-BUF · XRGB2101010 · {}×{} · stride {} · modifier 0x{modifier:016x}\n         Packed samples: first=0x{first:08x}, last=0x{last:08x}",
        WIDTH, HEIGHT, stride
    );
    println!("{details}");
    Ok((dmabuf_texture, cpu_texture, details))
}

fn make_picture(texture: &gdk4::Texture) -> Picture {
    let picture = Picture::new();
    picture.set_paintable(Some(texture));
    picture.set_content_fit(gtk4::ContentFit::Contain);
    picture.set_hexpand(true);
    picture.set_vexpand(true);
    picture
}

fn main() -> glib::ExitCode {
    let app = Application::builder()
        .application_id("com.github.Mel34.Glimpse.HDRProbe")
        .build();

    app.connect_activate(|app| {
        let window = ApplicationWindow::builder()
            .application(app)
            .title("Glimpse — HDR / DMA-BUF comparison")
            .default_width(1400)
            .default_height(800)
            .build();

        let content = GtkBox::new(Orientation::Vertical, 10);
        content.set_margin_top(12);
        content.set_margin_bottom(12);
        content.set_margin_start(12);
        content.set_margin_end(12);

        match build_pq_textures(&gtk4::prelude::WidgetExt::display(&window)) {
            Ok((dmabuf_texture, cpu_texture, details)) => {
                let images = GtkBox::new(Orientation::Horizontal, 10);
                let dmabuf_column = GtkBox::new(Orientation::Vertical, 4);
                let cpu_column = GtkBox::new(Orientation::Vertical, 4);
                dmabuf_column.append(&Label::new(Some("GBM DMA-BUF texture")));
                dmabuf_column.append(&make_picture(&dmabuf_texture));
                cpu_column.append(&Label::new(Some("System-memory reference (same bytes)")));
                cpu_column.append(&make_picture(&cpu_texture));
                images.append(&dmabuf_column);
                images.append(&cpu_column);
                content.append(&images);

                let explanation = Label::new(Some(&format!(
                    "Both images use identical XRGB2101010 pixel data and Rec. 2100 PQ color state.\n                     BT.2020 / ST 2084 PQ · left to right: {} cd/m²\n                     {details}\n                     If the right image appears and the left is black, the remaining fault is specific to DMA-BUF sampling/import.",
                    LEVELS_NITS.iter().map(|v| v.to_string()).collect::<Vec<_>>().join(" · ")
                )));
                explanation.set_wrap(true);
                explanation.set_selectable(true);
                content.append(&explanation);
            }
            Err(error) => {
                let message = format!("HDR texture comparison unavailable: {error}");
                eprintln!("{message}");
                content.append(&Label::new(Some(&message)));
            }
        }

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
