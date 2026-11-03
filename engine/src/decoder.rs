//! Software video decoding through the optional in-process FFmpeg backend.

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

            let timestamp = decoded.pts().or_else(|| decoded.timestamp());
            let pts = timestamp
                .map(|value| {
                    value as f64 * backend.time_base_num as f64
                        / backend.time_base_den as f64
                })
                .unwrap_or(0.0);
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
                .map_err(|_| "Decoded frame queue was aborted or frame exceeded its byte capacity".to_string())?;
        }
        Ok(())
    }

    #[cfg(not(feature = "ffmpeg-system"))]
    pub fn decode_step(&mut self, _target_serial: Serial) -> Result<bool, String> {
        Err("Software video decoding requires the ffmpeg-system feature".to_string())
    }
}
