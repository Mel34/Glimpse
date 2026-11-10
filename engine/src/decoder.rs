//! Software audio and video decoding through the optional in-process FFmpeg backend.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;
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
    time_base_num: i32,
    time_base_den: i32,
    frame_duration: f64,
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

        let context = ffmpeg::codec::context::Context::from_parameters(stream.parameters())
            .map_err(|error| format!("Could not create video decoder context: {error}"))?;
        let decoder = context
            .decoder()
            .video()
            .map_err(|error| format!("Could not open software video decoder: {error}"))?;

        let mut result = Self::new(packet_queue, frame_queue, clock);
        result.backend = Some(FfmpegDecoderState {
            decoder,
            scaler: None,
            time_base_num: time_base.numerator(),
            time_base_den: time_base.denominator(),
            frame_duration,
        });
        Ok(result)
    }

    /// Consumes at most one compressed packet and queues every frame made
    /// available by that packet. Call repeatedly from a decoder worker thread.
    #[cfg(feature = "ffmpeg-system")]
    pub fn decode_step(&mut self, target_serial: Serial) -> Result<bool, String> {
        use ffmpeg_the_third as ffmpeg;

        let Some(tagged_packet) = self.packet_queue.pop_timeout(Duration::from_millis(20)) else {
            return Ok(false);
        };
        if tagged_packet.serial != target_serial {
            return Ok(true);
        }

        let backend = self.backend.as_mut().ok_or_else(|| {
            "VideoDecoder was not initialised with VideoDecoder::open_ffmpeg".to_string()
        })?;
        let mut packet = ffmpeg::Packet::copy(&tagged_packet.item.data);
        packet.set_pts(tagged_packet.item.pts);
        packet.set_dts(tagged_packet.item.dts);
        packet.set_duration(tagged_packet.item.duration);
        packet.set_stream(tagged_packet.item.stream_index);
        backend
            .decoder
            .send_packet(&packet)
            .map_err(|error| format!("Could not submit compressed video packet: {error}"))?;

        Self::drain_frames(backend, &self.frame_queue, tagged_packet.serial)?;
        Ok(true)
    }

    /// Signals end-of-stream and emits delayed frames still held by the codec.
    #[cfg(feature = "ffmpeg-system")]
    pub fn finish(&mut self, serial: Serial) -> Result<(), String> {
        let backend = self.backend.as_mut().ok_or_else(|| {
            "VideoDecoder was not initialised with VideoDecoder::open_ffmpeg".to_string()
        })?;
        backend
            .decoder
            .send_eof()
            .map_err(|error| format!("Could not flush video decoder: {error}"))?;
        Self::drain_frames(backend, &self.frame_queue, serial)
    }

    #[cfg(feature = "ffmpeg-system")]
    fn drain_frames(
        backend: &mut FfmpegDecoderState,
        frame_queue: &VideoFrameQueue,
        serial: Serial,
    ) -> Result<(), String> {
        use ffmpeg_the_third as ffmpeg;

        loop {
            let mut decoded = ffmpeg::frame::Video::empty();
            match backend.decoder.receive_frame(&mut decoded) {
                Ok(()) => {}
                Err(ffmpeg::Error::Eof) => break,
                Err(ffmpeg::Error::Other { errno }) if errno == libc::EAGAIN => break,
                Err(error) => return Err(format!("Video frame decoding failed: {error}")),
            }

            // timestamp() reads FFmpeg's best_effort_timestamp, so it is
            // already the fallback when the frame's explicit PTS is absent.
            // If neither timestamp is available, drop this frame rather than
            // aborting otherwise playable media or inventing a sync position.
            let Some(timestamp) = select_video_timestamp(decoded.pts(), decoded.timestamp()) else {
                tracing::warn!("Skipping decoded video frame without PTS or best-effort timestamp");
                continue;
            };

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

            let pts = timestamp as f64 * backend.time_base_num as f64
                / backend.time_base_den as f64;
            let frame = VideoFrameData {
                width,
                height,
                stride,
                rgba_data: Arc::new(pixels),
                pts,
                duration: backend.frame_duration,
                serial,
            };
            frame_queue
                .push(frame, serial, stride * height as usize)
                .map_err(|_| "Decoded frame queue was aborted".to_string())?;
        }
        Ok(())
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
    pub text: String,
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
        let mut lines = Vec::new();

        for index in 0..raw.num_rects as usize {
            let rect = unsafe { *raw.rects.add(index) };
            if rect.is_null() { continue; }
            let rect = unsafe { &*rect };
            let text = unsafe { c_string_to_owned(rect.text) };
            let text = match text {
                Some(text) if !text.trim().is_empty() => Some(text),
                _ => unsafe { c_string_to_owned(rect.ass) }
                    .map(|ass| clean_ass_dialogue(&ass)),
            };
            if let Some(text) = text {
                let text = text.trim();
                if !text.is_empty() { lines.push(text.to_owned()); }
            }
        }

        let text = lines.join("\n");
        // AVSubtitle owns its rectangle array and each rectangle's text/ASS
        // buffers. The wrapper intentionally does not own those allocations.
        unsafe { ffmpeg::ffi::avsubtitle_free(decoded.as_mut_ptr()) };
        if text.is_empty() { return Ok(None); }
        Ok(Some(SubtitleCue { start, end: end.max(start), text }))
    }
}

#[cfg(feature = "ffmpeg-system")]
unsafe fn c_string_to_owned(pointer: *const libc::c_char) -> Option<String> {
    if pointer.is_null() { return None; }
    Some(std::ffi::CStr::from_ptr(pointer).to_string_lossy().into_owned())
}

#[cfg(feature = "ffmpeg-system")]
fn clean_ass_dialogue(ass: &str) -> String {
    let dialogue = ass.split_once("Dialogue:")
        .map(|(_, value)| value)
        .unwrap_or(ass);
    let mut fields = dialogue.splitn(10, ',');
    for _ in 0..9 { let _ = fields.next(); }
    let text = fields.next().unwrap_or(dialogue);
    let mut output = String::with_capacity(text.len());
    let mut in_override = false;
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '{' => in_override = true,
            '}' if in_override => in_override = false,
            '\\' if !in_override => match chars.next() {
                Some('N' | 'n') => output.push('\n'),
                Some('h') => output.push(' '),
                Some(other) => { output.push('\\'); output.push(other); }
                None => output.push('\\'),
            },
            _ if !in_override => output.push(ch),
            _ => {}
        }
    }
    output
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

        let context = ffmpeg::codec::context::Context::from_parameters(stream.parameters())
            .map_err(|error| format!("Could not create audio decoder context: {error}"))?;
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
    use super::clean_ass_dialogue;

    #[test]
    fn extracts_ass_dialogue_and_removes_override_tags() {
        let ass = r#"Dialogue: 0,0:00:01.00,0:00:03.00,Default,,0,0,0,,{\an8}Hello\Nworld"#;
        assert_eq!(clean_ass_dialogue(ass), "Hello\nworld");
    }

    #[test]
    fn replaces_ass_hard_spaces() {
        let ass = r#"0,0:00:00.00,0:00:02.00,Default,,0,0,0,,One\hTwo"#;
        assert_eq!(clean_ass_dialogue(ass), "One Two");
    }
}
