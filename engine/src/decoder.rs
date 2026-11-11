//! Software audio and video decoding through the optional in-process FFmpeg backend.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;
#[cfg(feature = "ffmpeg-system")]
use std::time::Instant;
#[cfg(feature = "ffmpeg-system")]
use std::time::Duration;

use crate::clock::MasterClock;
use crate::demuxer::DemuxPacket;
use crate::presenter::VideoFrameData;
use crate::queue::BoundedQueue;
use crate::types::Serial;

pub type VideoFrameQueue = BoundedQueue<VideoFrameData>;

/// Prefer the decoder's explicit PTS, falling back to FFmpeg's best-effort timestamp.
#[cfg(any(test, feature = "ffmpeg-system"))]
fn select_video_timestamp(pts: Option<i64>, best_effort_timestamp: Option<i64>) -> Option<i64> {
    pts.or(best_effort_timestamp)
}

#[cfg(test)]
mod video_timestamp_tests {
    use super::select_video_timestamp;

    #[test]
    fn explicit_pts_takes_precedence_over_best_effort_timestamp() {
        assert_eq!(select_video_timestamp(Some(42), Some(43)), Some(42));
    }

    #[test]
    fn best_effort_timestamp_is_used_when_pts_is_missing() {
        assert_eq!(select_video_timestamp(None, Some(43)), Some(43));
    }

    #[test]
    fn zero_pts_is_preserved() {
        assert_eq!(select_video_timestamp(Some(0), Some(43)), Some(0));
    }

    #[test]
    fn frame_is_skipped_when_both_timestamps_are_missing() {
        assert_eq!(select_video_timestamp(None, None), None);
    }
}

#[cfg(feature = "ffmpeg-system")]
struct FfmpegDecoderState {
    decoder: ffmpeg_the_third::decoder::Video,
    scaler: Option<ffmpeg_the_third::software::scaling::Context>,
    vaapi_enabled: bool,
    packets_submitted: u64,
    first_packet_submitted_at: Option<Instant>,
    first_frame_logged: bool,
    minimum_pts: Option<f64>,
    pre_target_frames_dropped: u64,
    first_pre_target_frame_pts: Option<f64>,
    last_pre_target_frame_pts: Option<f64>,
    first_target_frame_logged: bool,
    path: String,
    stream_index: usize,
    time_base_num: i32,
    time_base_den: i32,
    frame_duration: f64,
    seek_diagnostics: Option<SeekDecodeDiagnostics>,
    hw_frames_context_logged: bool,
}

#[cfg(feature = "ffmpeg-system")]
struct SeekDecodeDiagnostics {
    started_at: Instant,
    packet_wait_us_total: u64,
    packet_wait_us_max: u64,
    packet_wait_timeouts: u64,
    batches: u64,
    packets: u64,
    batch_size_max: usize,
    queue_depth_samples: u64,
    queue_depth_total: u64,
    queue_depth_max: usize,
    send_calls: u64,
    send_successes: u64,
    send_eagain: u64,
    send_us_total: u64,
    send_us_max: u64,
    receive_calls: u64,
    receive_successes: u64,
    receive_eagain: u64,
    receive_eof: u64,
    receive_us_total: u64,
    receive_us_max: u64,
    frames_received: u64,
}

#[cfg(feature = "ffmpeg-system")]
impl SeekDecodeDiagnostics {
    fn new() -> Self {
        Self {
            started_at: Instant::now(), packet_wait_us_total: 0, packet_wait_us_max: 0,
            packet_wait_timeouts: 0, batches: 0, packets: 0, batch_size_max: 0,
            queue_depth_samples: 0, queue_depth_total: 0, queue_depth_max: 0,
            send_calls: 0, send_successes: 0, send_eagain: 0, send_us_total: 0, send_us_max: 0,
            receive_calls: 0, receive_successes: 0, receive_eagain: 0, receive_eof: 0,
            receive_us_total: 0, receive_us_max: 0, frames_received: 0,
        }
    }

    fn record_packet_wait(&mut self, elapsed: Duration, timed_out: bool) {
        let us = elapsed.as_micros() as u64;
        self.packet_wait_us_total += us;
        self.packet_wait_us_max = self.packet_wait_us_max.max(us);
        self.packet_wait_timeouts += u64::from(timed_out);
    }

    fn observe_queue_depth(&mut self, depth: usize) {
        self.queue_depth_samples += 1;
        self.queue_depth_total += depth as u64;
        self.queue_depth_max = self.queue_depth_max.max(depth);
    }

    fn record_send(&mut self, elapsed: Duration, succeeded: bool, would_block: bool) {
        let us = elapsed.as_micros() as u64;
        self.send_calls += 1;
        self.send_successes += u64::from(succeeded);
        self.send_eagain += u64::from(would_block);
        self.send_us_total += us;
        self.send_us_max = self.send_us_max.max(us);
    }

    fn record_receive(&mut self, elapsed: Duration, received_frame: bool, would_block: bool, eof: bool) {
        let us = elapsed.as_micros() as u64;
        self.receive_calls += 1;
        self.receive_successes += u64::from(received_frame);
        self.receive_eagain += u64::from(would_block);
        self.receive_eof += u64::from(eof);
        self.receive_us_total += us;
        self.receive_us_max = self.receive_us_max.max(us);
        self.frames_received += u64::from(received_frame);
    }

    fn log_summary(&self, target_secs: f64, first_eligible_frame_pts: f64) {
        tracing::info!(
            target_secs, first_eligible_frame_pts,
            elapsed_us = self.started_at.elapsed().as_micros() as u64,
            packet_wait_us_total = self.packet_wait_us_total,
            packet_wait_us_max = self.packet_wait_us_max,
            packet_wait_timeouts = self.packet_wait_timeouts,
            batches = self.batches, packets = self.packets,
            average_batch_size = if self.batches == 0 { 0.0 } else { self.packets as f64 / self.batches as f64 },
            batch_size_max = self.batch_size_max,
            packet_queue_depth_average = if self.queue_depth_samples == 0 { 0.0 } else { self.queue_depth_total as f64 / self.queue_depth_samples as f64 },
            packet_queue_depth_max = self.queue_depth_max,
            send_calls = self.send_calls, send_successes = self.send_successes,
            send_eagain = self.send_eagain, send_us_total = self.send_us_total,
            send_us_average = if self.send_calls == 0 { 0.0 } else { self.send_us_total as f64 / self.send_calls as f64 },
            send_us_max = self.send_us_max,
            receive_calls = self.receive_calls, receive_successes = self.receive_successes,
            receive_eagain = self.receive_eagain, receive_eof = self.receive_eof,
            receive_us_total = self.receive_us_total,
            receive_us_average = if self.receive_calls == 0 { 0.0 } else { self.receive_us_total as f64 / self.receive_calls as f64 },
            receive_us_max = self.receive_us_max, frames_received = self.frames_received,
            "Seek decode performance summary"
        );
    }
}

#[cfg(feature = "ffmpeg-system")]
unsafe extern "C" fn select_vaapi_pixel_format(
    _context: *mut ffmpeg_the_third::ffi::AVCodecContext,
    formats: *const ffmpeg_the_third::ffi::AVPixelFormat,
) -> ffmpeg_the_third::ffi::AVPixelFormat {
    use ffmpeg_the_third::ffi::AVPixelFormat;
    if formats.is_null() {
        return AVPixelFormat::NONE;
    }
    let mut current = formats;
    loop {
        let format = unsafe { *current };
        if format == AVPixelFormat::NONE {
            break;
        }
        if format == AVPixelFormat::VAAPI {
            return format;
        }
        current = unsafe { current.add(1) };
    }
    AVPixelFormat::NONE
}

#[cfg(feature = "ffmpeg-system")]
fn open_software_video_decoder(
    path: &str,
    stream_index: usize,
) -> Result<ffmpeg_the_third::decoder::Video, String> {
    use ffmpeg_the_third as ffmpeg;

    let input = ffmpeg::format::input(path)
        .map_err(|error| format!("Could not reopen media for software video decoding: {error}"))?;
    let stream = input
        .streams()
        .find(|stream| stream.index() == stream_index)
        .ok_or_else(|| format!("Video stream {stream_index} was not found during software fallback"))?;
    let time_base = stream.time_base();
    if time_base.denominator() == 0 {
        return Err("Video stream has an invalid time base during software fallback".to_string());
    }
    let mut context = ffmpeg::codec::context::Context::from_parameters(stream.parameters())
        .map_err(|error| format!("Could not create software video decoder context: {error}"))?;
    unsafe {
        (*context.as_mut_ptr()).pkt_timebase = ffmpeg::ffi::AVRational {
            num: time_base.numerator(),
            den: time_base.denominator(),
        };
    }
    context
        .decoder()
        .video()
        .map_err(|error| format!("Could not open software video decoder: {error}"))
}

pub struct VideoDecoder {
    pub packet_queue: Arc<BoundedQueue<DemuxPacket>>,
    pub frame_queue: Arc<VideoFrameQueue>,
    pub clock: Arc<MasterClock>,
    pub running: Arc<AtomicBool>,
    #[cfg(feature = "ffmpeg-system")]
    backend: Option<FfmpegDecoderState>,
}

impl VideoDecoder {
    /// Creates an unconfigured decoder shell. Use open_ffmpeg() when the
    /// optional FFmpeg backend is enabled.
    pub fn new(
        packet_queue: Arc<BoundedQueue<DemuxPacket>>,
        frame_queue: Arc<VideoFrameQueue>,
        clock: Arc<MasterClock>,
    ) -> Self {
        Self {
            packet_queue,
            frame_queue,
            clock,
            running: Arc::new(AtomicBool::new(true)),
            #[cfg(feature = "ffmpeg-system")]
            backend: None,
        }
    }

    /// Opens the selected video stream's codec using its container parameters.
    /// The input is opened only long enough to copy codec parameters; compressed
    /// packets are still supplied through packet_queue by the demux worker.
    #[cfg(feature = "ffmpeg-system")]
    pub fn open_ffmpeg(
        path: &str,
        stream_index: usize,
        packet_queue: Arc<BoundedQueue<DemuxPacket>>,
        frame_queue: Arc<VideoFrameQueue>,
        clock: Arc<MasterClock>,
    ) -> Result<Self, String> {
        use ffmpeg_the_third as ffmpeg;

        let decoder_open_started_at = Instant::now();
        tracing::info!(stream_index, "Opening video decoder");
        ffmpeg::init().map_err(|error| format!("Could not initialise FFmpeg: {error}"))?;
        let input = ffmpeg::format::input(path)
            .map_err(|error| format!("Could not open video stream: {error}"))?;
        let stream = input
            .streams()
            .find(|stream| stream.index() == stream_index)
            .ok_or_else(|| format!("Video stream {stream_index} was not found"))?;

        let time_base = stream.time_base();
        if time_base.denominator() == 0 {
            return Err("Video stream has an invalid time base".to_string());
        }
        let frame_rate = stream.avg_frame_rate();
        let frame_duration = if frame_rate.numerator() > 0 && frame_rate.denominator() > 0 {
            frame_rate.denominator() as f64 / frame_rate.numerator() as f64
        } else {
            0.0
        };

        let use_vaapi = match std::env::var("GLIMPSE_VIDEO_DECODER") {
            Ok(value) if value.eq_ignore_ascii_case("software") => false,
            Ok(value) if value.eq_ignore_ascii_case("vaapi") => true,
            Ok(value) => {
                tracing::warn!(
                    %value,
                    "Unrecognized GLIMPSE_VIDEO_DECODER value; defaulting to VAAPI"
                );
                true
            }
            Err(std::env::VarError::NotPresent) => true,
            Err(error) => {
                tracing::warn!(
                    %error,
                    "Could not read GLIMPSE_VIDEO_DECODER; defaulting to VAAPI"
                );
                true
            }
        };
        let mut context = ffmpeg::codec::context::Context::from_parameters(stream.parameters())
            .map_err(|error| format!("Could not create video decoder context: {error}"))?;
        // Packet timestamps are expressed in the demuxed stream time base.
        // FFmpeg needs this explicitly for decoder-side timestamp adjustment.
        unsafe {
            (*context.as_mut_ptr()).pkt_timebase = ffmpeg::ffi::AVRational {
                num: time_base.numerator(),
                den: time_base.denominator(),
            };
        }

        let mut vaapi_enabled = use_vaapi;
        if use_vaapi {
            let setup_result = (|| -> Result<(), String> {
                let render_node = std::env::var("GLIMPSE_DRM_RENDER_NODE").ok();
                let render_node = render_node
                    .as_deref()
                    .map(std::ffi::CString::new)
                    .transpose()
                    .map_err(|_| "GLIMPSE_DRM_RENDER_NODE contains an interior NUL byte".to_string())?;
                let device_name = render_node.as_ref().map_or(std::ptr::null(), |value| value.as_ptr());
                let mut device: *mut ffmpeg::ffi::AVBufferRef = std::ptr::null_mut();
                let result = unsafe {
                    ffmpeg::ffi::av_hwdevice_ctx_create(
                        &mut device,
                        ffmpeg::ffi::AVHWDeviceType::VAAPI,
                        device_name,
                        std::ptr::null_mut(),
                        0,
                    )
                };
                if result < 0 {
                    return Err(format!("Could not create VAAPI device (FFmpeg error {result})"));
                }

                let context_ptr = unsafe { context.as_mut_ptr() };
                let device_ref = unsafe { ffmpeg::ffi::av_buffer_ref(device) };
                unsafe { ffmpeg::ffi::av_buffer_unref(&mut device) };
                if device_ref.is_null() {
                    return Err("Could not retain the VAAPI device context".to_string());
                }
                unsafe {
                    (*context_ptr).get_format = Some(select_vaapi_pixel_format);
                    (*context_ptr).hw_device_ctx = device_ref;
                }
                Ok(())
            })();

            if let Err(error) = setup_result {
                tracing::warn!(%error, "VAAPI setup failed; falling back to software video decoding");
                vaapi_enabled = false;
            }
        }

        let decoder = if vaapi_enabled {
            match context.decoder().video() {
                Ok(decoder) => decoder,
                Err(error) => {
                    tracing::warn!(%error, "Could not open VAAPI decoder; falling back to software video decoding");
                    vaapi_enabled = false;
                    open_software_video_decoder(path, stream_index)?
                }
            }
        } else {
            context
                .decoder()
                .video()
                .map_err(|error| format!("Could not open software video decoder: {error}"))?
        };

        let mut result = Self::new(packet_queue, frame_queue, clock);
        result.backend = Some(FfmpegDecoderState {
            decoder,
            scaler: None,
            vaapi_enabled,
            packets_submitted: 0,
            first_packet_submitted_at: None,
            first_frame_logged: false,
            minimum_pts: None,
            pre_target_frames_dropped: 0,
            first_pre_target_frame_pts: None,
            last_pre_target_frame_pts: None,
            first_target_frame_logged: false,
            path: path.to_owned(),
            stream_index,
            time_base_num: time_base.numerator(),
            time_base_den: time_base.denominator(),
            frame_duration,
            seek_diagnostics: None,
            hw_frames_context_logged: false,
        });
        tracing::info!(
            stream_index,
            vaapi_enabled,
            open_us = decoder_open_started_at.elapsed().as_micros() as u64,
            "Video decoder opened"
        );
        Ok(result)
    }

    /// Discard decoded frames before this media timestamp inside the decoder,
    /// before allocating presentation frames or converting pixel formats.
    #[cfg(feature = "ffmpeg-system")]
    pub fn set_minimum_pts(&mut self, minimum_pts: f64) {
        if let Some(backend) = self.backend.as_mut() {
            backend.minimum_pts = Some(minimum_pts);
            backend.seek_diagnostics = Some(SeekDecodeDiagnostics::new());
        }
    }

    /// Feeds a small packet batch before draining decoded frames. This keeps
    /// asynchronous hardware decoders supplied with work instead of forcing a
    /// receive/synchronisation round-trip after every compressed packet.
    #[cfg(feature = "ffmpeg-system")]
    pub fn decode_step(&mut self, target_serial: Serial) -> Result<bool, String> {
        use ffmpeg_the_third as ffmpeg;

        const MAX_PACKETS_PER_BATCH: usize = 8;

        let packet_wait_started_at = Instant::now();
        let first_packet = self.packet_queue.pop_timeout(Duration::from_millis(20));
        let packet_wait = packet_wait_started_at.elapsed();
        let Some(first_packet) = first_packet else {
            if let Some(diagnostics) = self.backend.as_mut().and_then(|backend| backend.seek_diagnostics.as_mut()) {
                diagnostics.record_packet_wait(packet_wait, true);
            }
            return Ok(false);
        };
        if first_packet.serial != target_serial {
            return Ok(true);
        }

        let backend = self.backend.as_mut().ok_or_else(|| {
            "VideoDecoder was not initialised with VideoDecoder::open_ffmpeg".to_string()
        })?;
        if let Some(diagnostics) = backend.seek_diagnostics.as_mut() {
            diagnostics.record_packet_wait(packet_wait, false);
            diagnostics.batches += 1;
            diagnostics.observe_queue_depth(self.packet_queue.stats().0);
        }
        let mut packets_in_batch = 0;
        let mut tagged_packet = Some(first_packet);

        while let Some(tagged) = tagged_packet.take() {
            let first_packet = backend.packets_submitted == 0;
            let packet_pts = tagged.item.pts_secs();
            let packet_keyframe = tagged.item.is_keyframe;
            let packet_bytes = tagged.item.data.len();
            let packet_started_at = Instant::now();
            let mut packet = ffmpeg::Packet::copy(&tagged.item.data);
            packet.set_pts(tagged.item.pts);
            packet.set_dts(tagged.item.dts);
            packet.set_duration(tagged.item.duration);
            packet.set_stream(tagged.item.stream_index);

            let mut vaapi_error = None;
            loop {
                let send_started_at = Instant::now();
                let send_result = backend.decoder.send_packet(&packet);
                if let Some(diagnostics) = backend.seek_diagnostics.as_mut() {
                    diagnostics.record_send(
                        send_started_at.elapsed(),
                        send_result.is_ok(),
                        matches!(&send_result, Err(ffmpeg::Error::Other { errno }) if *errno == libc::EAGAIN),
                    );
                }
                match send_result {
                    Ok(()) => break,
                    Err(ffmpeg::Error::Other { errno }) if errno == libc::EAGAIN => {
                        if let Some(diagnostics) = backend.seek_diagnostics.as_mut() {
                            diagnostics.batch_size_max = diagnostics.batch_size_max.max(packets_in_batch);
                        }
                        let drained = Self::drain_frames(
                            backend,
                            &self.frame_queue,
                            tagged.serial,
                        )?;
                        if drained == 0 {
                            return Err(
                                "Video decoder requested output draining but produced no frames"
                                    .to_string(),
                            );
                        }
                    }
                    Err(error) if backend.vaapi_enabled && backend.packets_submitted == 0 => {
                        let error_message = error.to_string();
                        tracing::warn!(
                            %error,
                            "VAAPI rejected the first compressed video packet; retrying with software decoding"
                        );
                        backend.decoder =
                            open_software_video_decoder(&backend.path, backend.stream_index)?;
                        backend.vaapi_enabled = false;
                        backend.scaler = None;
                        vaapi_error = Some(error_message);
                    }
                    Err(error) => {
                        return Err(match vaapi_error {
                            Some(vaapi_error) => format!(
                                "VAAPI rejected the first packet ({vaapi_error}); software fallback also failed: {error}"
                            ),
                            None => format!("Could not submit compressed video packet: {error}"),
                        });
                    }
                }
            }

            backend.packets_submitted += 1;
            packets_in_batch += 1;
            if let Some(diagnostics) = backend.seek_diagnostics.as_mut() {
                diagnostics.packets += 1;
                diagnostics.observe_queue_depth(self.packet_queue.stats().0);
            }
            if first_packet {
                backend.first_packet_submitted_at = Some(Instant::now());
                tracing::info!(
                    packet_pts_secs = packet_pts,
                    keyframe = packet_keyframe,
                    packet_bytes,
                    send_us = packet_started_at.elapsed().as_micros() as u64,
                    "Submitted first video packet to decoder"
                );
            }

            if packets_in_batch >= MAX_PACKETS_PER_BATCH {
                break;
            }
            tagged_packet = self.packet_queue.pop_timeout(Duration::ZERO)
                .filter(|next| next.serial == target_serial);
        }

        if let Some(diagnostics) = backend.seek_diagnostics.as_mut() {
            diagnostics.batch_size_max = diagnostics.batch_size_max.max(packets_in_batch);
        }
        Self::drain_frames(backend, &self.frame_queue, target_serial)?;
        Ok(true)
    }

    /// Signals end-of-stream and emits delayed frames still held by the codec.
    #[cfg(feature = "ffmpeg-system")]
    pub fn finish(&mut self, serial: Serial) -> Result<(), String> {
        use ffmpeg_the_third as ffmpeg;

        let backend = self.backend.as_mut().ok_or_else(|| {
            "VideoDecoder was not initialised with VideoDecoder::open_ffmpeg".to_string()
        })?;
        loop {
            match backend.decoder.send_eof() {
                Ok(()) => break,
                Err(ffmpeg::Error::Other { errno }) if errno == libc::EAGAIN => {
                    let drained = Self::drain_frames(backend, &self.frame_queue, serial)?;
                    if drained == 0 {
                        return Err(
                            "Video decoder requested output draining while flushing but produced no frames"
                                .to_string(),
                        );
                    }
                }
                Err(error) => {
                    return Err(format!("Could not flush video decoder: {error}"));
                }
            }
        }
        Self::drain_frames(backend, &self.frame_queue, serial)?;
        Ok(())
    }

    #[cfg(feature = "ffmpeg-system")]
    fn drain_frames(
        backend: &mut FfmpegDecoderState,
        frame_queue: &VideoFrameQueue,
        serial: Serial,
    ) -> Result<usize, String> {
        use ffmpeg_the_third as ffmpeg;

        let mut frames_received = 0;
        loop {
            let mut decoded = ffmpeg::frame::Video::empty();
            let receive_started_at = Instant::now();
            let receive_result = backend.decoder.receive_frame(&mut decoded);
            if let Some(diagnostics) = backend.seek_diagnostics.as_mut() {
                diagnostics.record_receive(
                    receive_started_at.elapsed(),
                    matches!(&receive_result, Ok(())),
                    matches!(&receive_result, Err(ffmpeg::Error::Other { errno }) if *errno == libc::EAGAIN),
                    matches!(&receive_result, Err(ffmpeg::Error::Eof)),
                );
            }
            match receive_result {
                Ok(()) => frames_received += 1,
                Err(ffmpeg::Error::Eof) => break,
                Err(ffmpeg::Error::Other { errno }) if errno == libc::EAGAIN => break,
                Err(error) => return Err(format!("Video frame decoding failed: {error}")),
            }

            if !backend.first_frame_logged {
                backend.first_frame_logged = true;
                tracing::info!(
                    frame_pts = decoded.pts(),
                    best_effort_timestamp = decoded.timestamp(),
                    elapsed_since_first_packet_us = backend.first_packet_submitted_at
                        .map(|started| started.elapsed().as_micros() as u64),
                    pixel_format = ?decoded.format(),
                    width = decoded.width(),
                    height = decoded.height(),
                    "Decoder produced first video frame"
                );
            }

            if !backend.hw_frames_context_logged {
                backend.hw_frames_context_logged = true;
                let codec_context = unsafe { &*backend.decoder.as_ptr() };
                tracing::info!(
                    thread_count = codec_context.thread_count,
                    thread_type = codec_context.thread_type,
                    active_thread_type = codec_context.active_thread_type,
                    "Video decoder runtime threading configuration"
                );

                let frame = unsafe { &*decoded.as_ptr() };
                if !frame.hw_frames_ctx.is_null() {
                    let frames_buffer = unsafe { &*frame.hw_frames_ctx };
                    let frames_context = frames_buffer.data as *const ffmpeg::ffi::AVHWFramesContext;
                    if !frames_context.is_null() {
                        let frames = unsafe { &*frames_context };
                        tracing::info!(
                            hardware_format = ?frames.format,
                            software_format = ?frames.sw_format,
                            surface_width = frames.width,
                            surface_height = frames.height,
                            initial_pool_size = frames.initial_pool_size,
                            "Negotiated VAAPI hardware frames context"
                        );
                    } else {
                        tracing::warn!("VAAPI frame has a hardware frames reference without context data");
                    }
                } else if backend.vaapi_enabled {
                    tracing::warn!("VAAPI decoder produced a frame without hw_frames_ctx");
                }
            }

            // timestamp() reads FFmpeg's best_effort_timestamp, so it is
            // already the fallback when the frame's explicit PTS is absent.
            // If neither timestamp is available, drop this frame rather than
            // aborting otherwise playable media or inventing a sync position.
            let Some(timestamp) = select_video_timestamp(decoded.pts(), decoded.timestamp()) else {
                tracing::warn!("Skipping decoded video frame without PTS or best-effort timestamp");
                continue;
            };

            let pts = timestamp as f64 * backend.time_base_num as f64
                / backend.time_base_den as f64;
            if let Some(minimum_pts) = backend.minimum_pts {
                if pts + 0.000_001 < minimum_pts {
                    backend.pre_target_frames_dropped += 1;
                    backend.first_pre_target_frame_pts.get_or_insert(pts);
                    backend.last_pre_target_frame_pts = Some(pts);
                    continue;
                }
                if !backend.first_target_frame_logged {
                    backend.first_target_frame_logged = true;
                    if let Some(diagnostics) = backend.seek_diagnostics.as_ref() {
                        diagnostics.log_summary(minimum_pts, pts);
                    }
                    tracing::info!(
                        target_secs = minimum_pts,
                        first_eligible_frame_pts = pts,
                        pre_target_frames_dropped = backend.pre_target_frames_dropped,
                        first_pre_target_frame_pts = backend.first_pre_target_frame_pts,
                        last_pre_target_frame_pts = backend.last_pre_target_frame_pts,
                        "Decoder reached seek target after discarding pre-target frames"
                    );
                }
            }

            if backend.vaapi_enabled && decoded.format() == ffmpeg::format::Pixel::VAAPI {
                let width = decoded.width();
                let height = decoded.height();
                let frame = VideoFrameData {
                    width,
                    height,
                    stride: 0,
                    rgba_data: Arc::new(Vec::new()),
                    vaapi_frame: Some(Arc::new(decoded)),
                    pts,
                    duration: backend.frame_duration,
                    serial,
                };
                let estimated_bytes = (width as usize)
                    .saturating_mul(height as usize)
                    .saturating_mul(3);
                frame_queue
                    .push(frame, serial, estimated_bytes)
                    .map_err(|_| "Decoded frame queue was aborted".to_string())?;
                continue;
            }

            let needs_scaler = backend.scaler.as_ref().map_or(true, |scaler| {
                scaler.input().format != decoded.format()
                    || scaler.input().width != decoded.width()
                    || scaler.input().height != decoded.height()
            });
            if needs_scaler {
                backend.scaler = Some(
                    ffmpeg::software::scaling::Context::get(
                        decoded.format(),
                        decoded.width(),
                        decoded.height(),
                        ffmpeg::format::Pixel::RGBA,
                        decoded.width(),
                        decoded.height(),
                        ffmpeg::software::scaling::Flags::BILINEAR,
                    )
                    .map_err(|error| format!("Could not initialise RGBA frame conversion: {error}"))?,
                );
            }

            let mut rgba = ffmpeg::frame::Video::empty();
            backend
                .scaler
                .as_mut()
                .expect("scaler was initialised")
                .run(&decoded, &mut rgba)
                .map_err(|error| format!("Could not convert decoded frame to RGBA: {error}"))?;

            let width = rgba.width();
            let height = rgba.height();
            let stride = width as usize * 4;
            let source_stride = rgba.stride(0);
            let source = rgba.data(0);
            let mut pixels = vec![0u8; stride * height as usize];
            for row in 0..height as usize {
                let source_start = row * source_stride;
                let target_start = row * stride;
                pixels[target_start..target_start + stride]
                    .copy_from_slice(&source[source_start..source_start + stride]);
            }

            let frame = VideoFrameData {
                width,
                height,
                stride,
                rgba_data: Arc::new(pixels),
                vaapi_frame: None,
                pts,
                duration: backend.frame_duration,
                serial,
            };
            frame_queue
                .push(frame, serial, stride * height as usize)
                .map_err(|_| "Decoded frame queue was aborted".to_string())?;
        }
        Ok(frames_received)
    }

    #[cfg(not(feature = "ffmpeg-system"))]
    pub fn decode_step(&mut self, _target_serial: Serial) -> Result<bool, String> {
        Err("Software video decoding requires the ffmpeg-system feature".to_string())
    }
}


// -----------------------------------------------------------------------------
// Subtitle decoding
// -----------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct SubtitleCue {
    pub start: f64,
    pub end: f64,
    /// Plain text retained for diagnostics and non-GTK consumers.
    pub text: String,
    /// Formatting-preserving spans; renderers decide how to display the styles.
    pub spans: Vec<SubtitleSpan>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SubtitleSpan {
    pub text: String,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strikethrough: bool,
}

#[cfg(feature = "ffmpeg-system")]
pub struct SubtitleDecoder {
    decoder: ffmpeg_the_third::codec::decoder::Subtitle,
    stream_index: usize,
}

#[cfg(feature = "ffmpeg-system")]
impl SubtitleDecoder {
    pub fn open_ffmpeg(path: &str, stream_index: usize) -> Result<Self, String> {
        use ffmpeg_the_third as ffmpeg;

        ffmpeg::init().map_err(|error| format!("Could not initialise FFmpeg: {error}"))?;
        let input = ffmpeg::format::input(path)
            .map_err(|error| format!("Could not open subtitle stream: {error}"))?;
        let stream = input.streams().find(|stream| stream.index() == stream_index)
            .ok_or_else(|| format!("Subtitle stream {stream_index} was not found"))?;
        let time_base = stream.time_base();
        if time_base.denominator() == 0 {
            return Err("Subtitle stream has an invalid time base".to_string());
        }
        let context = ffmpeg::codec::context::Context::from_parameters(stream.parameters())
            .map_err(|error| format!("Could not create subtitle decoder context: {error}"))?;
        let decoder = context.decoder().subtitle()
            .map_err(|error| format!("Could not open subtitle decoder: {error}"))?;
        Ok(Self { decoder, stream_index })
    }

    /// Decode one packet using FFmpeg 9's subtitle-specific synchronous API.
    /// Text is copied before avsubtitle_free releases FFmpeg's rectangle data.
    pub fn decode_packet(&mut self, packet: &DemuxPacket) -> Result<Option<SubtitleCue>, String> {
        use ffmpeg_the_third as ffmpeg;

        if packet.stream_index != self.stream_index {
            return Ok(None);
        }
        let mut compressed = ffmpeg::Packet::copy(&packet.data);
        compressed.set_pts(packet.pts);
        compressed.set_dts(packet.dts);
        compressed.set_duration(packet.duration);
        compressed.set_stream(packet.stream_index);

        let mut decoded = ffmpeg::Subtitle::new();
        let got_subtitle = self.decoder.decode(&compressed, &mut decoded)
            .map_err(|error| format!("Subtitle decoding failed: {error}"))?;
        if !got_subtitle {
            return Ok(None);
        }

        let raw = unsafe { &*decoded.as_ptr() };
        let base = if raw.pts != ffmpeg::ffi::AV_NOPTS_VALUE {
            raw.pts as f64 / ffmpeg::ffi::AV_TIME_BASE as f64
        } else {
            packet.pts_secs().unwrap_or(0.0)
        };
        let start = base + raw.start_display_time as f64 / 1000.0;
        let decoded_end = base + raw.end_display_time as f64 / 1000.0;
        let packet_duration = if packet.time_base_den != 0 {
            packet.duration.max(0) as f64 * packet.time_base_num as f64 / packet.time_base_den as f64
        } else { 0.0 };
        let end = if decoded_end > base + raw.start_display_time as f64 / 1000.0 {
            decoded_end
        } else {
            base + raw.start_display_time as f64 / 1000.0 + packet_duration
        };
        let mut spans = Vec::new();

        for index in 0..raw.num_rects as usize {
            let rect = unsafe { *raw.rects.add(index) };
            if rect.is_null() { continue; }
            let rect = unsafe { &*rect };

            // Prefer the ASS event payload when FFmpeg supplies it: rect.text
            // may already have had all inline style information stripped.
            let ass = unsafe { c_string_to_owned(rect.ass) };
            let rect_spans = match ass.filter(|value| !value.trim().is_empty()) {
                Some(ass) => parse_ass_dialogue(&ass),
                None => unsafe { c_string_to_owned(rect.text) }
                    .filter(|value| !value.trim().is_empty())
                    .map(|text| vec![SubtitleSpan {
                        text,
                        // Preserve Glimpse's existing bold SRT/plain-text appearance.
                        bold: true,
                        italic: false,
                        underline: false,
                        strikethrough: false,
                    }])
                    .unwrap_or_default(),
            };

            if rect_spans.iter().any(|span| !span.text.is_empty()) {
                if !spans.is_empty() {
                    spans.push(SubtitleSpan {
                        text: "\n".to_string(),
                        bold: false,
                        italic: false,
                        underline: false,
                        strikethrough: false,
                    });
                }
                spans.extend(rect_spans);
            }
        }

        let text = spans.iter().map(|span| span.text.as_str()).collect::<String>();
        // AVSubtitle owns its rectangle array and each rectangle's text/ASS
        // buffers. The wrapper intentionally does not own those allocations.
        unsafe { ffmpeg::ffi::avsubtitle_free(decoded.as_mut_ptr()) };
        if text.trim().is_empty() { return Ok(None); }
        Ok(Some(SubtitleCue { start, end: end.max(start), text, spans }))
    }
}

#[cfg(feature = "ffmpeg-system")]
unsafe fn c_string_to_owned(pointer: *const libc::c_char) -> Option<String> {
    if pointer.is_null() { return None; }
    Some(std::ffi::CStr::from_ptr(pointer).to_string_lossy().into_owned())
}

#[cfg(feature = "ffmpeg-system")]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct AssTextStyle {
    bold: bool,
    italic: bool,
    underline: bool,
    strikethrough: bool,
}

#[cfg(feature = "ffmpeg-system")]
fn append_ass_text(spans: &mut Vec<SubtitleSpan>, text: &str, style: AssTextStyle) {
    if text.is_empty() {
        return;
    }
    if let Some(last) = spans.last_mut() {
        if last.bold == style.bold
            && last.italic == style.italic
            && last.underline == style.underline
            && last.strikethrough == style.strikethrough
        {
            last.text.push_str(text);
            return;
        }
    }
    spans.push(SubtitleSpan {
        text: text.to_owned(),
        bold: style.bold,
        italic: style.italic,
        underline: style.underline,
        strikethrough: style.strikethrough,
    });
}

#[cfg(feature = "ffmpeg-system")]
fn apply_ass_override(block: &str, style: &mut AssTextStyle) {
    for command in block.split('\\').filter(|command| !command.is_empty()) {
        let mut chars = command.chars();
        let Some(tag) = chars.next() else { continue };
        let value = chars.as_str().trim();
        match tag {
            'b' => {
                if let Ok(weight) = value.parse::<i32>() {
                    style.bold = weight != 0;
                }
            }
            'i' => {
                if let Ok(enabled) = value.parse::<i32>() {
                    style.italic = enabled != 0;
                }
            }
            'u' => {
                if let Ok(enabled) = value.parse::<i32>() {
                    style.underline = enabled != 0;
                }
            }
            's' => {
                if let Ok(enabled) = value.parse::<i32>() {
                    style.strikethrough = enabled != 0;
                }
            }
            _ => {}
        }
    }
}

#[cfg(feature = "ffmpeg-system")]
fn parse_ass_dialogue(ass: &str) -> Vec<SubtitleSpan> {
    // FFmpeg's AVSubtitleRect.ass omits the Dialogue: prefix and timestamps,
    // leaving the nine-field ASS event form. Full Dialogue lines have ten.
    let (dialogue, fields_to_skip) = match ass.split_once("Dialogue:") {
        Some((_, value)) => (value, 9),
        None => (ass, 8),
    };
    let mut fields = dialogue.splitn(fields_to_skip + 1, ',');
    for _ in 0..fields_to_skip {
        let _ = fields.next();
    }
    let text = fields.next().unwrap_or(dialogue);

    let mut spans = Vec::new();
    let mut style = AssTextStyle::default();
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '{' => {
                let mut block = String::new();
                let mut closed = false;
                for next in chars.by_ref() {
                    if next == '}' {
                        closed = true;
                        break;
                    }
                    block.push(next);
                }
                if closed {
                    apply_ass_override(&block, &mut style);
                } else {
                    append_ass_text(&mut spans, "{", style);
                    append_ass_text(&mut spans, &block, style);
                }
            }
            '\\' => match chars.next() {
                Some('N' | 'n') => append_ass_text(&mut spans, "\n", style),
                Some('h') => append_ass_text(&mut spans, "\u{00a0}", style),
                Some('{') => append_ass_text(&mut spans, "{", style),
                Some('}') => append_ass_text(&mut spans, "}", style),
                Some(other) => {
                    let mut literal = String::from("\\");
                    literal.push(other);
                    append_ass_text(&mut spans, &literal, style);
                }
                None => append_ass_text(&mut spans, "\\", style),
            },
            _ => {
                let mut character = String::new();
                character.push(ch);
                append_ass_text(&mut spans, &character, style);
            }
        }
    }
    spans
}

/// Decode a standalone text subtitle file into timestamped cues.
///
/// The file is demuxed independently from the media container. Only the first
/// subtitle stream is used; bitmap-only tracks naturally produce no text cues.
#[cfg(feature = "ffmpeg-system")]
pub fn decode_external_subtitle_file(path: &str) -> Result<Vec<SubtitleCue>, String> {
    use ffmpeg_the_third as ffmpeg;

    ffmpeg::init().map_err(|error| format!("Could not initialise FFmpeg: {error}"))?;
    let mut input = ffmpeg::format::input(path)
        .map_err(|error| format!("Could not open external subtitle file: {error}"))?;

    let (stream_index, time_base_num, time_base_den, decoder) = {
        let stream = input
            .streams()
            .best(ffmpeg::media::Type::Subtitle)
            .ok_or_else(|| "The selected file contains no subtitle stream".to_string())?;
        let time_base = stream.time_base();
        if time_base.denominator() == 0 {
            return Err("The external subtitle stream has an invalid time base".to_string());
        }
        let context = ffmpeg::codec::context::Context::from_parameters(stream.parameters())
            .map_err(|error| format!("Could not create external subtitle decoder context: {error}"))?;
        let decoder = context
            .decoder()
            .subtitle()
            .map_err(|error| format!("Could not open external subtitle decoder: {error}"))?;
        (stream.index(), time_base.numerator(), time_base.denominator(), decoder)
    };

    let mut subtitle_decoder = SubtitleDecoder { decoder, stream_index };
    let mut cues = Vec::new();
    for packet_result in input.packets() {
        let (stream, packet) = packet_result
            .map_err(|error| format!("Could not read external subtitle packet: {error}"))?;
        if stream.index() != stream_index {
            continue;
        }
        let subtitle_packet = DemuxPacket {
            stream_index,
            data: packet.data().unwrap_or_default().to_vec(),
            pts: packet.pts(),
            dts: packet.dts(),
            duration: packet.duration(),
            time_base_num,
            time_base_den,
            is_keyframe: packet.is_key(),
        };
        if let Some(cue) = subtitle_decoder.decode_packet(&subtitle_packet)? {
            cues.push(cue);
        }
    }

    cues.sort_by(|left, right| left.start.total_cmp(&right.start));
    if cues.is_empty() {
        return Err(
            "No readable text subtitles were found. Bitmap subtitles are not supported yet."
                .to_string(),
        );
    }
    Ok(cues)
}

// -----------------------------------------------------------------------------
// Audio decoding
// -----------------------------------------------------------------------------

/// Decoded audio is normalised to interleaved stereo f32 at 48 kHz.
/// The output queue is a worker-to-worker handoff; it is deliberately not
/// suitable for direct use from a PipeWire real-time callback.
pub const AUDIO_OUTPUT_RATE: u32 = 48_000;
pub const AUDIO_OUTPUT_CHANNELS: usize = 2;

#[derive(Clone)]
pub struct AudioFrameData {
    pub samples: Arc<Vec<f32>>,
    pub pts: f64,
    pub duration: f64,
    pub serial: Serial,
}

pub type AudioFrameQueue = BoundedQueue<AudioFrameData>;

#[cfg(feature = "ffmpeg-system")]
struct FfmpegAudioDecoderState {
    decoder: ffmpeg_the_third::decoder::Audio,
    resampler: Option<ffmpeg_the_third::software::resampling::Context>,
    time_base_num: i32,
    time_base_den: i32,
    stream_index: usize,
    next_pts: f64,
    discard_before_pts: f64,
}

#[cfg(feature = "ffmpeg-system")]
fn normalized_input_layout<'a>(
    input_layout: &ffmpeg_the_third::ChannelLayout<'a>,
) -> ffmpeg_the_third::ChannelLayout<'a> {
    use ffmpeg_the_third as ffmpeg;

    match input_layout.order() {
        ffmpeg::util::channel_layout::ChannelOrder::Unspecified => {
            ffmpeg::ChannelLayout::default_for_channels(input_layout.channels())
        }
        _ => input_layout.clone(),
    }
}

pub struct AudioDecoder {
    pub packet_queue: Arc<BoundedQueue<DemuxPacket>>,
    pub frame_queue: Arc<AudioFrameQueue>,
    #[cfg(feature = "ffmpeg-system")]
    backend: Option<FfmpegAudioDecoderState>,
}

impl AudioDecoder {
    /// Opens the selected audio stream's codec. Compressed packets are supplied
    /// separately through packet_queue by the demux worker.
    #[cfg(feature = "ffmpeg-system")]
    pub fn open_ffmpeg(
        path: &str,
        stream_index: usize,
        packet_queue: Arc<BoundedQueue<DemuxPacket>>,
        frame_queue: Arc<AudioFrameQueue>,
    ) -> Result<Self, String> {
        use ffmpeg_the_third as ffmpeg;

        ffmpeg::init().map_err(|error| format!("Could not initialise FFmpeg: {error}"))?;
        let input = ffmpeg::format::input(path)
            .map_err(|error| format!("Could not open audio stream: {error}"))?;
        let stream = input
            .streams()
            .find(|stream| stream.index() == stream_index)
            .ok_or_else(|| format!("Audio stream {stream_index} was not found"))?;

        let time_base = stream.time_base();
        if time_base.denominator() == 0 {
            return Err("Audio stream has an invalid time base".to_string());
        }

        let mut context = ffmpeg::codec::context::Context::from_parameters(stream.parameters())
            .map_err(|error| format!("Could not create audio decoder context: {error}"))?;
        // In particular, AC3 skip-sample side data needs pkt_timebase so
        // FFmpeg can adjust decoded timestamps when samples are discarded.
        unsafe {
            (*context.as_mut_ptr()).pkt_timebase = ffmpeg::ffi::AVRational {
                num: time_base.numerator(),
                den: time_base.denominator(),
            };
        }
        let decoder = context
            .decoder()
            .audio()
            .map_err(|error| format!("Could not open software audio decoder: {error}"))?;

        Ok(Self {
            packet_queue,
            frame_queue,
            backend: Some(FfmpegAudioDecoderState {
                decoder,
                resampler: None,
                time_base_num: time_base.numerator(),
                time_base_den: time_base.denominator(),
                stream_index,
                next_pts: 0.0,
                discard_before_pts: 0.0,
            }),
        })
    }

    /// Discards decoded audio samples whose timestamps precede the requested
    /// playback position. Seeking begins at a demuxer keyframe, so this keeps
    /// the audible start aligned with the user's target.
    #[cfg(feature = "ffmpeg-system")]
    pub fn discard_before_pts(&mut self, pts: f64) {
        if let Some(backend) = self.backend.as_mut() {
            let pts = pts.max(0.0);
            backend.discard_before_pts = pts;
            backend.next_pts = pts;
        }
    }

    /// Consumes at most one compressed packet and queues all PCM frames made
    /// available by it. Call repeatedly from a decoder worker thread.
    #[cfg(feature = "ffmpeg-system")]
    pub fn decode_step(&mut self, target_serial: Serial) -> Result<bool, String> {
        use ffmpeg_the_third as ffmpeg;
        use std::time::Duration;

        let Some(tagged_packet) = self.packet_queue.pop_timeout(Duration::from_millis(20)) else {
            return Ok(false);
        };
        if tagged_packet.serial != target_serial {
            return Ok(true);
        }

        let backend = self.backend.as_mut().ok_or_else(|| {
            "AudioDecoder was not initialised with AudioDecoder::open_ffmpeg".to_string()
        })?;
        if tagged_packet.item.stream_index != backend.stream_index {
            return Ok(true);
        }
        let mut packet = ffmpeg::Packet::copy(&tagged_packet.item.data);
        packet.set_pts(tagged_packet.item.pts);
        packet.set_dts(tagged_packet.item.dts);
        packet.set_duration(tagged_packet.item.duration);
        packet.set_stream(tagged_packet.item.stream_index);
        backend
            .decoder
            .send_packet(&packet)
            .map_err(|error| format!("Could not submit compressed audio packet: {error}"))?;

        Self::drain_frames(backend, &self.frame_queue, tagged_packet.serial)?;
        Ok(true)
    }

    /// Drains delayed decoder and resampler samples at end of stream.
    #[cfg(feature = "ffmpeg-system")]
    pub fn finish(&mut self, serial: Serial) -> Result<(), String> {
        let backend = self.backend.as_mut().ok_or_else(|| {
            "AudioDecoder was not initialised with AudioDecoder::open_ffmpeg".to_string()
        })?;
        backend
            .decoder
            .send_eof()
            .map_err(|error| format!("Could not flush audio decoder: {error}"))?;
        Self::drain_frames(backend, &self.frame_queue, serial)?;
        Self::flush_resampler(backend, &self.frame_queue, serial)
    }

    #[cfg(feature = "ffmpeg-system")]
    fn drain_frames(
        backend: &mut FfmpegAudioDecoderState,
        frame_queue: &AudioFrameQueue,
        serial: Serial,
    ) -> Result<(), String> {
        use ffmpeg_the_third as ffmpeg;

        loop {
            let mut decoded = ffmpeg::frame::Audio::empty();
            match backend.decoder.receive_frame(&mut decoded) {
                Ok(()) => {}
                Err(ffmpeg::Error::Eof) => break,
                Err(ffmpeg::Error::Other { errno }) if errno == libc::EAGAIN => break,
                Err(error) => return Err(format!("Audio frame decoding failed: {error}")),
            }

            let input_layout = decoded.ch_layout();
            let source_layout: ffmpeg::ChannelLayout<'static> =
                normalized_input_layout(&input_layout).into_owned().into();
            drop(input_layout);
            // swresample validates the frame layout against its configured input.
            // Carry the inferred default layout on the frame as well.
            decoded.set_ch_layout(source_layout.clone());
            let input_mask = source_layout.mask().ok_or_else(|| {
                format!(
                    "Audio stream uses an unsupported channel layout: {}",
                    source_layout.description()
                )
            })?;
            let needs_resampler = backend.resampler.as_ref().map_or(true, |resampler| {
                resampler.input().format != decoded.format()
                    || resampler.input().channel_layout != input_mask
                    || resampler.input().rate != decoded.rate()
            });

            if needs_resampler {
                backend.resampler = Some(
                    ffmpeg::software::resampling::Context::get2(
                        decoded.format(),
                        source_layout,
                        decoded.rate(),
                        ffmpeg::format::Sample::F32(ffmpeg::format::sample::Type::Packed),
                        ffmpeg::ChannelLayout::STEREO.clone(),
                        AUDIO_OUTPUT_RATE,
                    )
                    .map_err(|error| format!("Could not initialise audio resampler: {error}"))?,
                );
            }

            let output_capacity = decoded
                .samples()
                .saturating_mul(AUDIO_OUTPUT_RATE as usize)
                .div_ceil(decoded.rate().max(1) as usize)
                .saturating_add(64)
                .max(1);
            let stereo_mask = ffmpeg::ChannelLayout::STEREO
                .mask()
                .expect("stereo has a native channel mask");
            let mut converted = ffmpeg::frame::Audio::new(
                ffmpeg::format::Sample::F32(ffmpeg::format::sample::Type::Packed),
                output_capacity,
                stereo_mask,
            );
            backend
                .resampler
                .as_mut()
                .expect("resampler was initialised")
                .run(&decoded, &mut converted)
                .map_err(|error| format!("Could not resample decoded audio: {error}"))?;

            let timestamp = decoded.pts().or_else(|| decoded.timestamp());
            let pts = timestamp
                .map(|value| {
                    value as f64 * backend.time_base_num as f64
                        / backend.time_base_den as f64
                })
                .unwrap_or(backend.next_pts);
            Self::queue_converted_frame(backend, frame_queue, &converted, pts, serial)?;
        }

        Ok(())
    }

    #[cfg(feature = "ffmpeg-system")]
    fn queue_converted_frame(
        backend: &mut FfmpegAudioDecoderState,
        frame_queue: &AudioFrameQueue,
        converted: &ffmpeg_the_third::frame::Audio,
        pts: f64,
        serial: Serial,
    ) -> Result<(), String> {
        if converted.samples() == 0 {
            return Ok(());
        }

        let stereo = converted.plane::<(f32, f32)>(0);
        let original_duration = stereo.len() as f64 / AUDIO_OUTPUT_RATE as f64;
        let skip_frames = (if pts < backend.discard_before_pts {
            ((backend.discard_before_pts - pts) * AUDIO_OUTPUT_RATE as f64)
                .ceil()
                .max(0.0) as usize
        } else {
            0
        })
        .min(stereo.len());

        let frame_pts = pts + skip_frames as f64 / AUDIO_OUTPUT_RATE as f64;
        let mut samples = Vec::with_capacity(
            (stereo.len() - skip_frames) * AUDIO_OUTPUT_CHANNELS,
        );
        for (left, right) in stereo.iter().skip(skip_frames) {
            samples.push(*left);
            samples.push(*right);
        }

        let duration = samples.len() as f64
            / AUDIO_OUTPUT_CHANNELS as f64
            / AUDIO_OUTPUT_RATE as f64;
        backend.next_pts = pts + original_duration;
        if samples.is_empty() {
            return Ok(());
        }

        let byte_size = samples.len() * std::mem::size_of::<f32>();
        frame_queue
            .push(
                AudioFrameData {
                    samples: Arc::new(samples),
                    pts: frame_pts,
                    duration,
                    serial,
                },
                serial,
                byte_size,
            )
            .map_err(|_| "Decoded audio queue was aborted".to_string())?;
        Ok(())
    }

    #[cfg(feature = "ffmpeg-system")]
    fn flush_resampler(
        backend: &mut FfmpegAudioDecoderState,
        frame_queue: &AudioFrameQueue,
        serial: Serial,
    ) -> Result<(), String> {
        use ffmpeg_the_third as ffmpeg;

        let stereo_mask = ffmpeg::ChannelLayout::STEREO
            .mask()
            .expect("stereo has a native channel mask");
        loop {
            let Some(delay) = backend.resampler.as_ref().and_then(|resampler| resampler.delay()) else {
                break;
            };
            let capacity = delay.output.max(1) as usize + 32;
            let mut converted = ffmpeg::frame::Audio::new(
                ffmpeg::format::Sample::F32(ffmpeg::format::sample::Type::Packed),
                capacity,
                stereo_mask,
            );
            let remaining = backend
                .resampler
                .as_mut()
                .expect("resampler delay requires a resampler")
                .flush(&mut converted)
                .map_err(|error| format!("Could not flush audio resampler: {error}"))?;
            let pts = backend.next_pts;
            Self::queue_converted_frame(
                backend,
                frame_queue,
                &converted,
                pts,
                serial,
            )?;
            if remaining.is_none() {
                break;
            }
        }
        Ok(())
    }

    #[cfg(not(feature = "ffmpeg-system"))]
    pub fn decode_step(&mut self, _target_serial: Serial) -> Result<bool, String> {
        Err("Software audio decoding requires the ffmpeg-system feature".to_string())
    }
}


#[cfg(all(test, feature = "ffmpeg-system"))]
mod audio_layout_tests {
    use super::*;

    #[test]
    fn unspecified_mono_layout_uses_ffmpeg_default() {
        let input = ffmpeg_the_third::ChannelLayout::unspecified(1);
        let normalized = normalized_input_layout(&input);

        assert_eq!(normalized, ffmpeg_the_third::ChannelLayout::MONO);
    }

    #[test]
    fn unspecified_stereo_layout_uses_ffmpeg_default() {
        let input = ffmpeg_the_third::ChannelLayout::unspecified(2);
        let normalized = normalized_input_layout(&input);

        assert_eq!(normalized, ffmpeg_the_third::ChannelLayout::STEREO);
    }

    #[test]
    fn native_channel_layout_is_preserved() {
        let input = ffmpeg_the_third::ChannelLayout::STEREO;
        let normalized = normalized_input_layout(&input);

        assert_eq!(normalized, input);
    }
}


#[cfg(all(test, feature = "ffmpeg-system"))]
mod subtitle_text_tests {
    use super::{parse_ass_dialogue, SubtitleSpan};

    fn span(text: &str, bold: bool, italic: bool, underline: bool, strikethrough: bool) -> SubtitleSpan {
        SubtitleSpan {
            text: text.to_string(),
            bold,
            italic,
            underline,
            strikethrough,
        }
    }

    #[test]
    fn extracts_dialogue_and_preserves_line_breaks() {
        let ass = r#"Dialogue: 0,0:00:01.00,0:00:03.00,Default,,0,0,0,,Hello\Nworld"#;
        assert_eq!(parse_ass_dialogue(ass), vec![span("Hello\nworld", false, false, false, false)]);
    }

    #[test]
    fn parses_nine_field_ffmpeg_ass_payload() {
        let ass = r#"0,0,Default,,0,0,0,,One\hTwo"#;
        assert_eq!(parse_ass_dialogue(ass), vec![span("One\u{00a0}Two", false, false, false, false)]);
    }

    #[test]
    fn parses_individual_inline_style_tags_and_resets() {
        let ass = r#"0,0,Default,,0,0,0,,normal {\b1}bold{\b0} {\i1}italic{\i0} {\u1}underlined{\u0} {\s1}struck{\s0}done"#;
        assert_eq!(
            parse_ass_dialogue(ass),
            vec![
                span("normal ", false, false, false, false),
                span("bold", true, false, false, false),
                span(" ", false, false, false, false),
                span("italic", false, true, false, false),
                span(" ", false, false, false, false),
                span("underlined", false, false, true, false),
                span(" ", false, false, false, false),
                span("struck", false, false, false, true),
                span("done", false, false, false, false),
            ]
        );
    }

    #[test]
    fn combines_styles_and_keeps_active_style_across_unknown_tags() {
        let ass = r#"0,0,Default,,0,0,0,,{\b1\i1}both{\an8} still both{\b0}italic only{\i0}"#;
        assert_eq!(
            parse_ass_dialogue(ass),
            vec![
                span("both still both", true, true, false, false),
                span("italic only", false, true, false, false),
            ]
        );
    }

    #[test]
    fn ignores_unknown_override_commands() {
        let ass = r#"0,0,Default,,0,0,0,,before{\bord3\fs40}after"#;
        assert_eq!(parse_ass_dialogue(ass), vec![span("beforeafter", false, false, false, false)]);
    }
}