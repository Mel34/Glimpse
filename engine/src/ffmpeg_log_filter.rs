#[cfg(feature = "ffmpeg-system")]
unsafe extern "C" {
    fn glimpse_ffmpeg_install_log_filter();
}

/// Installs the process-wide FFmpeg callback before Glimpse opens any media.
/// The native callback forwards every message except duplicate UDTA fallback
/// warnings for a file that has already emitted one.
pub(crate) fn install() {
    unsafe {
        glimpse_ffmpeg_install_log_filter();
    }
}
