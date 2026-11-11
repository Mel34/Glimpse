//! GTK DMA-BUF texture import for FFmpeg VAAPI frames.

use ffmpeg_the_third::frame::Video;
use glib::object::{Cast, ObjectType};
use glib::translate::from_glib_full;
use std::ffi::{CStr, c_char, c_void};
use std::ptr;

unsafe extern "C" {
    fn glimpse_vaapi_texture_from_frame(
        frame: *const c_void,
        display: *mut c_void,
        error_message: *mut *mut c_char,
    ) -> *mut c_void;
}

pub(crate) fn create_texture(frame: &Video) -> Result<gdk4::Texture, String> {
    let display = gdk4::Display::default()
        .ok_or_else(|| "No GDK display is available for DMA-BUF import".to_string())?;
    let mut error_message = ptr::null_mut();
    let texture = unsafe {
        glimpse_vaapi_texture_from_frame(
            frame.as_ptr().cast(),
            display.as_ptr().cast(),
            &mut error_message,
        )
    };

    if texture.is_null() {
        let message = if error_message.is_null() {
            "GTK rejected the VAAPI DMA-BUF frame".to_string()
        } else {
            let message = unsafe { CStr::from_ptr(error_message) }
                .to_string_lossy()
                .into_owned();
            unsafe { glib::ffi::g_free(error_message.cast()) };
            message
        };
        return Err(message);
    }

    Ok(unsafe { from_glib_full(texture.cast()) })
}

/// Download a VAAPI surface and convert it to an ordinary GTK-compatible RGBA
/// texture. This is deliberately a presentation fallback: decoding remains
/// hardware accelerated, and the expensive readback is used only after the
/// DMA-BUF import path has failed for this playback run.
pub(crate) fn create_rgba_fallback_texture(frame: &Video) -> Result<gdk4::Texture, String> {
    use ffmpeg_the_third as ffmpeg;

    let mut downloaded = Video::empty();
    let transfer_result = unsafe {
        ffmpeg::ffi::av_hwframe_transfer_data(downloaded.as_mut_ptr(), frame.as_ptr(), 0)
    };
    if transfer_result < 0 {
        return Err(format!(
            "Could not download VAAPI frame for RGBA fallback (FFmpeg error {transfer_result})"
        ));
    }

    let width = downloaded.width();
    let height = downloaded.height();
    let mut scaler = ffmpeg::software::scaling::Context::get(
        downloaded.format(),
        width,
        height,
        ffmpeg::format::Pixel::RGBA,
        width,
        height,
        ffmpeg::software::scaling::Flags::BILINEAR,
    )
    .map_err(|error| format!("Could not initialise RGBA fallback conversion: {error}"))?;

    let mut rgba = Video::empty();
    scaler
        .run(&downloaded, &mut rgba)
        .map_err(|error| format!("Could not convert downloaded VAAPI frame to RGBA: {error}"))?;

    let width = rgba.width();
    let height = rgba.height();
    let stride = width as usize * 4;
    let source_stride = rgba.stride(0);
    let source = rgba.data(0);
    let mut pixels = vec![0_u8; stride * height as usize];
    for row in 0..height as usize {
        let source_start = row * source_stride;
        let target_start = row * stride;
        pixels[target_start..target_start + stride]
            .copy_from_slice(&source[source_start..source_start + stride]);
    }

    let bytes = glib::Bytes::from_owned(pixels);
    Ok(gdk4::MemoryTexture::new(
        width as i32,
        height as i32,
        gdk4::MemoryFormat::R8g8b8a8,
        &bytes,
        stride,
    )
    .upcast::<gdk4::Texture>())
}
