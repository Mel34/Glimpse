//! Static HDR DMA-BUF probe with byte-identical system-memory and SDR controls.

use gdk4::{ColorState, DmabufTextureBuilder, MemoryFormat, MemoryTextureBuilder};
use gbm::{BufferObject, BufferObjectFlags, Device, Format, Modifier};
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
// GTK 4.24 added GDK_MEMORY_XRGB2101010 as enum value 67. gtk-rs 0.9.6
// forwards unknown enum values unchanged through this compatibility variant.
const GDK_MEMORY_XRGB2101010: MemoryFormat = MemoryFormat::__Unknown(67);

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

/// P010 stores 10-bit samples in the most-significant bits of 16-bit words.
/// Luma uses the 10-bit limited/video range [64, 940].
fn p010_luma_code(nits: f64) -> u16 {
    let pq = pq_encode(nits) as u32;
    let code10 = 64 + (pq * 876 + 32_767) / 65_535;
    (code10 as u16) << 6
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
            let band = (x * LEVELS_NITS.len() / WIDTH as usize).min(LEVELS_NITS.len() - 1);
            let code16 = pq_encode(LEVELS_NITS[band]) as u32;
            let code10 = (code16 * 1023 + 32_767) / 65_535;
            // DRM_FORMAT_XRGB2101010: X[31:30], R[29:20], G[19:10], B[9:0].
            let packed = (3u32 << 30) | (code10 << 20) | (code10 << 10) | code10;
            row[x * 4..x * 4 + 4].copy_from_slice(&packed.to_ne_bytes());
        }
    }
}

fn fill_argb_pq_pattern(buffer: &mut [u8], stride: usize) {
    for y in 0..HEIGHT as usize {
        let row = &mut buffer[y * stride..(y + 1) * stride];
        for x in 0..WIDTH as usize {
            let band = (x * LEVELS_NITS.len() / WIDTH as usize).min(LEVELS_NITS.len() - 1);
            let code16 = pq_encode(LEVELS_NITS[band]) as u32;
            let code10 = (code16 * 1023 + 32_767) / 65_535;
            // DRM_FORMAT_ARGB2101010: A[31:30], R[29:20], G[19:10], B[9:0].
            let packed = (3u32 << 30) | (code10 << 20) | (code10 << 10) | code10;
            row[x * 4..x * 4 + 4].copy_from_slice(&packed.to_ne_bytes());
        }
    }
}

fn fill_sdr_control(buffer: &mut [u8], stride: usize) {
    for y in 0..HEIGHT as usize {
        let row = &mut buffer[y * stride..(y + 1) * stride];
        for x in 0..WIDTH as usize {
            let band = (x * LEVELS_NITS.len() / WIDTH as usize).min(LEVELS_NITS.len() - 1);
            let value = 24 + (band as u32 * 15);
            // DRM_FORMAT_XRGB8888, opaque neutral gray. GDK defaults to sRGB.
            let packed = (0xffu32 << 24) | (value << 16) | (value << 8) | value;
            row[x * 4..x * 4 + 4].copy_from_slice(&packed.to_ne_bytes());
        }
    }
}

fn fill_p010_y(buffer: &mut [u8], stride: usize, width: u32, height: u32) {
    for y in 0..height as usize {
        let row = &mut buffer[y * stride..(y + 1) * stride];
        for x in 0..width as usize {
            let band = (x * LEVELS_NITS.len() / width as usize).min(LEVELS_NITS.len() - 1);
            let sample = p010_luma_code(LEVELS_NITS[band]);
            row[x * 2..x * 2 + 2].copy_from_slice(&sample.to_le_bytes());
        }
    }
}

fn fill_p010_uv(buffer: &mut [u8], stride: usize, width: u32, height: u32) {
    // Neutral chroma (U=V=512) makes this a luma/PQ import test independent
    // of YCbCr matrix selection. P010 stores interleaved U then V words.
    let neutral = (512u16 << 6).to_le_bytes();
    for y in 0..height as usize {
        let row = &mut buffer[y * stride..(y + 1) * stride];
        for x in 0..width as usize {
            let offset = x * 4;
            row[offset..offset + 2].copy_from_slice(&neutral);
            row[offset + 2..offset + 4].copy_from_slice(&neutral);
        }
    }
}

fn verify_bo_contents_for<T: 'static>(
    bo: &BufferObject<T>,
    width: u32,
    height: u32,
    expected: &[u8],
) -> Result<String, Box<dyn Error>> {
    let result = stage(
        bo.map(0, 0, width, height, |mapping| {
            let actual = mapping.buffer();
            let compared = actual.len().min(expected.len());
            let mismatches = actual[..compared]
                .iter()
                .zip(&expected[..compared])
                .filter(|(a, b)| a != b)
                .count();
            let mut checksum = 0xcbf29ce484222325u64;
            for byte in &actual[..compared] {
                checksum ^= *byte as u64;
                checksum = checksum.wrapping_mul(0x100000001b3);
            }
            let first = actual.get(..4).unwrap_or(&[]);
            (actual.len(), compared, mismatches, checksum, first.to_vec())
        }),
        "read back GBM buffer after CPU write",
    )?;
    Ok(format!(
        "GBM readback: mapped={} bytes, compared={} bytes, mismatches={}, FNV-1a=0x{:016x}, first bytes={:02x?}",
        result.0, result.1, result.2, result.3, result.4
    ))
}

fn import_bo<T: 'static>(
    display: &gdk4::Display,
    bo: &BufferObject<T>,
    color_state: Option<&ColorState>,
) -> Result<gdk4::Texture, Box<dyn Error>> {
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
    builder.set_width(bo.width());
    builder.set_height(bo.height());
    builder.set_fourcc(bo.format() as u32);
    builder.set_modifier(modifier);
    builder.set_n_planes(1);
    builder.set_fd(0, raw_fd);
    builder.set_offset(0, bo.offset(0));
    builder.set_stride(0, bo.stride());
    if let Some(state) = color_state {
        builder.set_color_state(Some(state));
    }

    // Keep the exported fd alive until GDK finalizes the imported texture.
    stage(
        unsafe { builder.build_with_release_func(move || drop(dmabuf)) },
        "import DMA-BUF into GDK texture",
    )
}

fn import_p010(
    display: &gdk4::Display,
    y_bo: &BufferObject<()>,
    uv_bo: &BufferObject<()>,
) -> Result<gdk4::Texture, Box<dyn Error>> {
    let y_modifier = modifier_value(y_bo.modifier())?;
    let uv_modifier = modifier_value(uv_bo.modifier())?;
    if y_modifier != uv_modifier {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!("P010 planes have different modifiers: 0x{y_modifier:016x} vs 0x{uv_modifier:016x}"),
        )
        .into());
    }

    let y_fd = stage(y_bo.fd(), "export P010 Y plane DMA-BUF fd")?;
    let uv_fd = stage(uv_bo.fd(), "export P010 UV plane DMA-BUF fd")?;
    let y_raw_fd = y_fd.as_raw_fd();
    let uv_raw_fd = uv_fd.as_raw_fd();

    let builder = DmabufTextureBuilder::new();
    builder.set_display(display);
    builder.set_width(WIDTH);
    builder.set_height(HEIGHT);
    builder.set_fourcc(Format::P010 as u32);
    builder.set_modifier(y_modifier);
    builder.set_n_planes(2);
    builder.set_fd(0, y_raw_fd);
    builder.set_offset(0, y_bo.offset(0));
    builder.set_stride(0, y_bo.stride());
    builder.set_fd(1, uv_raw_fd);
    builder.set_offset(1, uv_bo.offset(0));
    builder.set_stride(1, uv_bo.stride());
    builder.set_color_state(Some(&ColorState::rec2100_pq()));

    // The P010 descriptor references two independently allocated DMA-BUF planes.
    // Keep both exported fds alive until GDK finalizes the imported texture.
    stage(
        unsafe { builder.build_with_release_func(move || drop((y_fd, uv_fd))) },
        "import two-plane P010 DMA-BUF into GDK texture",
    )
}

fn make_cpu_reference(pixels: Vec<u8>, stride: usize) -> Result<gdk4::Texture, Box<dyn Error>> {
    let bytes = glib::Bytes::from_owned(pixels);
    let builder = MemoryTextureBuilder::new();
    builder.set_width(WIDTH as i32);
    builder.set_height(HEIGHT as i32);
    builder.set_stride(stride);
    builder.set_format(GDK_MEMORY_XRGB2101010);
    builder.set_bytes(Some(&bytes));
    builder.set_color_state(Some(&ColorState::rec2100_pq()));
    Ok(builder.build())
}

fn allocate_plane<T: 'static>(
    gbm: &Device<std::fs::File>,
    format: Format,
    width: u32,
    height: u32,
    fill: impl FnOnce(&mut [u8], usize),
) -> Result<(BufferObject<T>, Vec<u8>), Box<dyn Error>>
where
    T: Default,
{
    let usage = BufferObjectFlags::RENDERING | BufferObjectFlags::LINEAR;
    if !gbm.is_format_supported(format, usage) {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!("GBM does not support linear renderable {format:?}"),
        )
        .into());
    }

    let mut bo = stage(
        gbm.create_buffer_object::<T>(width, height, format, usage),
        &format!("allocate linear renderable {format:?} GBM plane"),
    )?;
    let stride = bo.stride() as usize;
    let mut pixels = vec![0u8; stride * height as usize];
    fill(&mut pixels, stride);
    stage(
        bo.map_mut(0, 0, width, height, |mapping| {
            assert_eq!(mapping.stride() as usize, stride, "GBM mapped stride differs from BO stride");
            mapping.buffer_mut()[..pixels.len()].copy_from_slice(&pixels);
        }),
        &format!("map and write {format:?} GBM plane"),
    )?;
    Ok((bo, pixels))
}

fn allocate_pattern<T: 'static>(
    gbm: &Device<std::fs::File>,
    format: Format,
    fill: impl FnOnce(&mut [u8], usize),
) -> Result<(BufferObject<T>, Vec<u8>, usize), Box<dyn Error>>
where
    T: Default,
{
    let (bo, pixels) = allocate_plane(gbm, format, WIDTH, HEIGHT, fill)?;
    let stride = bo.stride() as usize;
    Ok((bo, pixels, stride))
}

fn build_textures(
    display: &gdk4::Display,
) -> Result<(gdk4::Texture, gdk4::Texture, gdk4::Texture, gdk4::Texture, gdk4::Texture, String), Box<dyn Error>> {
    let device_path = std::env::var("GLIMPSE_DRM_DEVICE")
        .unwrap_or_else(|_| "/dev/dri/renderD128".to_string());
    let drm_file = stage(
        OpenOptions::new().read(true).write(true).open(&device_path),
        &format!("open DRM device {device_path}"),
    )?;
    let gbm = stage(Device::new(drm_file), "create GBM device")?;

    let (pq_bo, pq_pixels, pq_stride) =
        allocate_pattern::<()>(&gbm, Format::Xrgb2101010, fill_pq_pattern)?;
    let pq_readback = verify_bo_contents_for(&pq_bo, WIDTH, HEIGHT, &pq_pixels)?;
    let pq_reference = make_cpu_reference(pq_pixels, pq_stride)?;
    let pq_texture = import_bo(display, &pq_bo, Some(&ColorState::rec2100_pq()))?;

    let (argb_bo, argb_pixels, argb_stride) =
        allocate_pattern::<()>(&gbm, Format::Argb2101010, fill_argb_pq_pattern)?;
    let argb_readback = verify_bo_contents_for(&argb_bo, WIDTH, HEIGHT, &argb_pixels)?;
    let argb_texture = import_bo(display, &argb_bo, Some(&ColorState::rec2100_pq()))?;

    // P010 is represented by a 16-bit Y plane and an interleaved 16-bit UV plane.
    // Separate linear GBM allocations let us populate and validate each plane
    // without assuming how a multi-planar GBM mapping exposes its backing storage.
    let (y_bo, y_pixels) = allocate_plane::<()>(
        &gbm,
        Format::R16,
        WIDTH,
        HEIGHT,
        |pixels, stride| fill_p010_y(pixels, stride, WIDTH, HEIGHT),
    )?;
    let (uv_bo, uv_pixels) = allocate_plane::<()>(
        &gbm,
        Format::Gr1616,
        WIDTH / 2,
        HEIGHT / 2,
        |pixels, stride| fill_p010_uv(pixels, stride, WIDTH / 2, HEIGHT / 2),
    )?;
    let y_readback = verify_bo_contents_for(&y_bo, WIDTH, HEIGHT, &y_pixels)?;
    let uv_readback =
        verify_bo_contents_for(&uv_bo, WIDTH / 2, HEIGHT / 2, &uv_pixels)?;
    let p010_texture = import_p010(display, &y_bo, &uv_bo)?;

    let (sdr_bo, sdr_pixels, _) =
        allocate_pattern::<()>(&gbm, Format::Xrgb8888, fill_sdr_control)?;
    let sdr_readback = verify_bo_contents_for(&sdr_bo, WIDTH, HEIGHT, &sdr_pixels)?;
    let sdr_texture = import_bo(display, &sdr_bo, None)?;

    let pq_modifier = modifier_value(pq_bo.modifier())?;
    let argb_modifier = modifier_value(argb_bo.modifier())?;
    let p010_y_modifier = modifier_value(y_bo.modifier())?;
    let p010_uv_modifier = modifier_value(uv_bo.modifier())?;
    let sdr_modifier = modifier_value(sdr_bo.modifier())?;
    let details = format!(
        "PQ XRGB2101010 · stride {} · modifier 0x{pq_modifier:016x}\n{pq_readback}\n         PQ ARGB2101010 · stride {} · modifier 0x{argb_modifier:016x}\n{argb_readback}\n         P010 · Y stride {} offset {} · UV stride {} offset {} · modifiers 0x{p010_y_modifier:016x}/0x{p010_uv_modifier:016x}\n         P010 Y plane: {y_readback}\nP010 UV plane: {uv_readback}\n         SDR XRGB8888 · stride {} · modifier 0x{sdr_modifier:016x}\n{sdr_readback}",
        pq_stride, argb_stride, y_bo.stride(), y_bo.offset(0),
        uv_bo.stride(), uv_bo.offset(0), sdr_bo.stride()
    );
    println!("{details}");
    Ok((pq_texture, pq_reference, argb_texture, p010_texture, sdr_texture, details))
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
            .default_width(1900)
            .default_height(800)
            .build();

        let content = GtkBox::new(Orientation::Vertical, 10);
        content.set_margin_top(12);
        content.set_margin_bottom(12);
        content.set_margin_start(12);
        content.set_margin_end(12);

        match build_textures(&gtk4::prelude::WidgetExt::display(&window)) {
            Ok((pq_dmabuf, pq_cpu, argb_dmabuf, p010_dmabuf, sdr_dmabuf, details)) => {
                let images = GtkBox::new(Orientation::Horizontal, 10);
                for (label, texture) in [
                    ("PQ DMA-BUF · XRGB2101010", &pq_dmabuf),
                    ("PQ system memory · same bytes", &pq_cpu),
                    ("PQ DMA-BUF · ARGB2101010", &argb_dmabuf),
                    ("PQ DMA-BUF · P010 two-plane YUV", &p010_dmabuf),
                    ("SDR DMA-BUF control · XRGB8888", &sdr_dmabuf),
                ] {
                    let column = GtkBox::new(Orientation::Vertical, 4);
                    column.set_hexpand(true);
                    column.append(&Label::new(Some(label)));
                    column.append(&make_picture(texture));
                    images.append(&column);
                }
                content.append(&images);

                let explanation = Label::new(Some(&format!(
                    "P010 uses limited-range 10-bit PQ luma and neutral interleaved UV (U=V=512), so the grayscale ramp tests YUV DMA-BUF import without depending on a YCbCr matrix.\n                     PQ bands, left to right: {} cd/m²\n{details}",
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
    use super::{p010_luma_code, pq_encode};

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

    #[test]
    fn p010_luma_uses_limited_range_and_msb_alignment() {
        assert_eq!(p010_luma_code(0.0), 64 << 6);
        assert_eq!(p010_luma_code(10_000.0), 940 << 6);
        assert!(p010_luma_code(203.0) > (64 << 6));
        assert_eq!(p010_luma_code(203.0) & 0x3f, 0);
    }
}
