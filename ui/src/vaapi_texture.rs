//! GTK DMA-BUF texture import for FFmpeg VAAPI frames.

use ffmpeg_the_third::frame::Video;
use glib::object::ObjectType;
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
