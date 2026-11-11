//! Container probing and in-process packet demuxing.

#[cfg(feature = "ffmpeg-system")]
use std::collections::HashMap;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::queue::BoundedQueue;
use crate::types::{Serial, TrackId, TrackInfo, TrackList, TrackType};

#[derive(Clone, Debug)]
pub struct DemuxPacket {
    pub stream_index: usize,
    pub data: Vec<u8>,
    pub pts: Option<i64>,
    pub dts: Option<i64>,
    pub duration: i64,
    pub time_base_num: i32,
    pub time_base_den: i32,
    pub is_keyframe: bool,
}

impl DemuxPacket {
    pub fn pts_secs(&self) -> Option<f64> {
        if self.time_base_den == 0 {
            return None;
        }
        self.pts.map(|p| (p as f64 * self.time_base_num as f64) / self.time_base_den as f64)
    }
}

pub enum DemuxerCommand {
    Seek { target: Duration, serial: Serial },
    Stop,
}

pub struct DemuxerState {
    pub current_serial: Serial,
    pub duration: Option<Duration>,
    pub tracks: TrackList,
}

/// Three independently bounded packet queues, one per supported stream type.
#[derive(Clone)]
pub struct PacketQueues {
    pub video: Arc<BoundedQueue<DemuxPacket>>,
    pub audio: Arc<BoundedQueue<DemuxPacket>>,
    pub subtitle: Arc<BoundedQueue<DemuxPacket>>,
}

impl PacketQueues {
    pub fn new(max_packets_per_queue: usize, max_bytes_per_queue: usize) -> Self {
        Self {
            video: Arc::new(BoundedQueue::new(max_packets_per_queue, max_bytes_per_queue)),
            audio: Arc::new(BoundedQueue::new(max_packets_per_queue, max_bytes_per_queue)),
            subtitle: Arc::new(BoundedQueue::new(max_packets_per_queue, max_bytes_per_queue)),
        }
    }

    pub fn flush(&self, serial: Serial) {
        self.video.flush(serial);
        self.audio.flush(serial);
        self.subtitle.flush(serial);
    }

    pub fn abort(&self) {
        self.video.abort();
        self.audio.abort();
        self.subtitle.abort();
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct DemuxStats {
    pub video_packets: u64,
    pub audio_packets: u64,
    pub subtitle_packets: u64,
    pub ignored_packets: u64,
    pub bytes_read: u64,
}

/// Open a media source with libavformat and route its packets into bounded queues.
///
/// This function performs blocking I/O and queue backpressure. Call it from a
/// dedicated worker thread, never from the GTK main thread. It returns after
/// clean EOF or on the first demux/queue error. Decoding is intentionally not
/// part of this milestone.
#[cfg(feature = "ffmpeg-system")]
pub fn demux_file(
    path: &str,
    tracks: &TrackList,
    queues: &PacketQueues,
    serial: Serial,
) -> Result<DemuxStats, String> {
    demux_file_from(path, tracks, queues, serial, Duration::ZERO)
}

/// Demuxes from the closest seek point at or before the requested media time.
/// Decoder-side timestamp trimming is responsible for discarding pre-target audio.
#[cfg(feature = "ffmpeg-system")]
pub fn demux_file_from(
    path: &str,
    tracks: &TrackList,
    queues: &PacketQueues,
    serial: Serial,
    start_position: Duration,
) -> Result<DemuxStats, String> {
    use ffmpeg_the_third as ffmpeg;

    let demux_started_at = Instant::now();
    tracing::info!(target_secs = start_position.as_secs_f64(), "Video demux worker starting");
    ffmpeg::init().map_err(|error| format!("Could not initialise FFmpeg: {error}"))?;
    let open_started_at = Instant::now();
    let mut input = ffmpeg::format::input(path)
        .map_err(|error| format!("Could not open media with libavformat: {error}"))?;
    tracing::info!(open_us = open_started_at.elapsed().as_micros() as u64, "Video demux input opened");
    if !start_position.is_zero() {
        let timestamp = i64::try_from(start_position.as_micros()).unwrap_or(i64::MAX);
        let seek_started_at = Instant::now();
        // Prefer a backward-bounded seek so decoding can trim from the
        // preceding keyframe. Some demuxers reject that constrained interval
        // even when they support ordinary timestamp seeking, so retry with
        // FFmpeg's unconstrained seek range.
        if let Err(backward_error) = input.seek(timestamp, ..timestamp) {
            input.seek(timestamp, ..).map_err(|fallback_error| {
                format!(
                    "Could not seek media to {:.3}s (backward seek: {}; fallback seek: {})",
                    start_position.as_secs_f64(),
                    backward_error,
                    fallback_error,
                )
            })?;
        }
        tracing::info!(
            target_secs = start_position.as_secs_f64(),
            seek_us = seek_started_at.elapsed().as_micros() as u64,
            elapsed_us = demux_started_at.elapsed().as_micros() as u64,
            "Video demux seek completed"
        );
    }

    let mut stream_types = HashMap::new();
    for track in tracks.video_tracks.iter() {
        stream_types.insert(track.id.0, TrackType::Video);
    }
    for track in tracks.audio_tracks.iter() {
        stream_types.insert(track.id.0, TrackType::Audio);
    }
    for track in tracks.subtitle_tracks.iter() {
        stream_types.insert(track.id.0, TrackType::Subtitle);
    }

    let mut stats = DemuxStats::default();
    let mut first_video_packet = true;

    for packet_result in input.packets() {
        let (stream, packet) = packet_result
            .map_err(|error| format!("FFmpeg packet read failed: {error}"))?;
        let stream_index = stream.index();
        let Some(stream_type) = stream_types.get(&stream_index).copied() else {
            stats.ignored_packets += 1;
            continue;
        };

        let time_base = stream.time_base();
        let data = packet.data().unwrap_or_default().to_vec();
        let byte_size = data.len();
        let demux_packet = DemuxPacket {
            stream_index,
            data,
            pts: packet.pts(),
            dts: packet.dts(),
            duration: packet.duration(),
            time_base_num: time_base.numerator(),
            time_base_den: time_base.denominator(),
            is_keyframe: packet.is_key(),
        };

        let queue = match stream_type {
            TrackType::Video => {
                stats.video_packets += 1;
                if first_video_packet {
                    first_video_packet = false;
                    tracing::info!(
                        target_secs = start_position.as_secs_f64(),
                        packet_pts_secs = demux_packet.pts_secs(),
                        keyframe = demux_packet.is_keyframe,
                        packet_bytes = byte_size,
                        elapsed_us = demux_started_at.elapsed().as_micros() as u64,
                        "Demux produced first video packet"
                    );
                }
                &queues.video
            }
            TrackType::Audio => {
                stats.audio_packets += 1;
                &queues.audio
            }
            TrackType::Subtitle => {
                stats.subtitle_packets += 1;
                &queues.subtitle
            }
        };

        queue.push(demux_packet, serial, byte_size).map_err(|_| {
            format!("Packet queue for stream {stream_index} was aborted")
        })?;
        stats.bytes_read = stats.bytes_read.saturating_add(byte_size as u64);
    }

    Ok(stats)
}

/// Metadata discovered from the actual media container.
#[derive(Debug, Clone, PartialEq)]
pub struct MediaProbe {
    pub duration: Option<Duration>,
    pub tracks: TrackList,
}

/// Run the system ffprobe utility and return real container metadata.
pub fn probe_media(path: &str) -> Result<MediaProbe, String> {
    if std::env::var_os("GLIMPSE_DEBUG").is_some() {
        tracing::info!("Probing media container: {}", path);
    }

    let output = Command::new("ffprobe")
        .args([
            "-v", "error",
            "-show_format",
            "-show_streams",
            "-show_entries",
            "format=duration:stream=index,codec_type,codec_name,width,height,sample_rate,channels:stream_tags=language,title:stream_disposition=default,attached_pic",
            "-of", "json",
        ])
        .arg(path)
        .output()
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                "Could not find ffprobe. Install FFmpeg and ensure ffprobe is on PATH.".to_string()
            } else {
                format!("Could not start ffprobe: {error}")
            }
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if stderr.is_empty() {
            format!("ffprobe failed with status {}", output.status)
        } else {
            format!("ffprobe could not read '{}': {}", path, stderr)
        });
    }

    let stdout = String::from_utf8(output.stdout)
        .map_err(|error| format!("ffprobe returned non-UTF-8 output: {error}"))?;
    parse_ffprobe_output(&stdout)
}

fn parse_ffprobe_output(json: &str) -> Result<MediaProbe, String> {
    let root: Value = serde_json::from_str(json)
        .map_err(|error| format!("Could not parse ffprobe output: {error}"))?;

    let duration = root
        .get("format")
        .and_then(|format| format.get("duration"))
        .and_then(value_as_f64)
        .filter(|seconds| seconds.is_finite() && *seconds >= 0.0)
        .and_then(|seconds| Duration::try_from_secs_f64(seconds).ok());

    let mut tracks = TrackList::default();
    let streams = root
        .get("streams")
        .and_then(Value::as_array)
        .ok_or_else(|| "ffprobe output did not contain a stream list".to_string())?;

    for stream in streams {
        let stream_type = match stream.get("codec_type").and_then(Value::as_str) {
            Some("video") => TrackType::Video,
            Some("audio") => TrackType::Audio,
            Some("subtitle") => TrackType::Subtitle,
            _ => continue,
        };

        let index = stream
            .get("index")
            .and_then(Value::as_u64)
            .and_then(|index| usize::try_from(index).ok())
            .ok_or_else(|| "ffprobe returned a stream without a valid index".to_string())?;

        let tags = stream.get("tags");
        let disposition = stream.get("disposition");
        let disposition_default = disposition
            .and_then(|disposition| disposition.get("default"))
            .and_then(Value::as_u64)
            .unwrap_or(0) != 0;
        let is_attached_picture = disposition
            .and_then(|disposition| disposition.get("attached_pic"))
            .and_then(Value::as_u64)
            .unwrap_or(0) != 0;

        // Album art is encoded as a video stream by some containers, but it
        // must not participate in playable video-track selection.
        if stream_type == TrackType::Video && is_attached_picture {
            continue;
        }

        let track = TrackInfo {
            id: TrackId(index),
            track_type: stream_type,
            title: tags
                .and_then(|tags| tags.get("title"))
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(str::to_owned),
            language: tags
                .and_then(|tags| tags.get("language"))
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(str::to_owned),
            codec_name: stream
                .get("codec_name")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_owned(),
            width: stream.get("width").and_then(Value::as_u64).and_then(|v| u32::try_from(v).ok()),
            height: stream.get("height").and_then(Value::as_u64).and_then(|v| u32::try_from(v).ok()),
            sample_rate: stream
                .get("sample_rate")
                .and_then(value_as_u64)
                .and_then(|v| u32::try_from(v).ok()),
            channels: stream
                .get("channels")
                .and_then(Value::as_u64)
                .and_then(|v| u16::try_from(v).ok()),
            is_default: disposition_default,
        };

        match stream_type {
            TrackType::Video => tracks.video_tracks.push(track),
            TrackType::Audio => tracks.audio_tracks.push(track),
            TrackType::Subtitle => tracks.subtitle_tracks.push(track),
        }
    }

    // Prefer the container's declared default video stream, falling back
    // to the first playable stream when no video default is marked.
    tracks.video_tracks.sort_by_key(|track| !track.is_default);

    tracks.active_audio = tracks
        .audio_tracks
        .iter()
        .find(|track| track.is_default)
        .or_else(|| tracks.audio_tracks.first())
        .map(|track| track.id);
    tracks.active_subtitle = None;

    Ok(MediaProbe { duration, tracks })
}

fn value_as_f64(value: &Value) -> Option<f64> {
    value.as_f64().or_else(|| value.as_str()?.parse::<f64>().ok())
}

fn value_as_u64(value: &Value) -> Option<u64> {
    value.as_u64().or_else(|| value.as_str()?.parse::<u64>().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_realistic_multistream_probe_output() {
        let json = r#"{
            "format": {"duration": "91.625"},
            "streams": [
                {
                    "index": 0, "codec_type": "video", "codec_name": "hevc",
                    "width": 3840, "height": 2160,
                    "disposition": {"default": 1}
                },
                {
                    "index": 1, "codec_type": "audio", "codec_name": "aac",
                    "sample_rate": "48000", "channels": 2,
                    "tags": {"language": "eng", "title": "Stereo"},
                    "disposition": {"default": 0}
                },
                {
                    "index": 2, "codec_type": "audio", "codec_name": "ac3",
                    "sample_rate": "48000", "channels": 6,
                    "tags": {"language": "deu"},
                    "disposition": {"default": 1}
                },
                {
                    "index": 3, "codec_type": "subtitle", "codec_name": "subrip",
                    "tags": {"language": "eng", "title": "Full subtitles"},
                    "disposition": {"default": 1}
                },
                {"index": 4, "codec_type": "data", "codec_name": "bin_data"}
            ]
        }"#;

        let probe = parse_ffprobe_output(json).expect("valid ffprobe JSON");
        assert_eq!(probe.duration, Some(Duration::from_millis(91_625)));
        assert_eq!(probe.tracks.video_tracks.len(), 1);
        assert_eq!(probe.tracks.video_tracks[0].width, Some(3840));
        assert_eq!(probe.tracks.video_tracks[0].height, Some(2160));
        assert_eq!(probe.tracks.audio_tracks.len(), 2);
        assert_eq!(probe.tracks.audio_tracks[0].title.as_deref(), Some("Stereo"));
        assert_eq!(probe.tracks.active_audio, Some(TrackId(2)));
        assert_eq!(probe.tracks.subtitle_tracks.len(), 1);
        assert_eq!(probe.tracks.active_subtitle, None);
    }

    #[test]
    fn excludes_attached_pictures_and_prefers_default_video_track() {
        let json = r#"{
            "format": {"duration": "12.0"},
            "streams": [
                {
                    "index": 0, "codec_type": "video", "codec_name": "mjpeg",
                    "width": 600, "height": 600,
                    "disposition": {"default": 0, "attached_pic": 1}
                },
                {
                    "index": 1, "codec_type": "video", "codec_name": "h264",
                    "width": 1920, "height": 1080,
                    "disposition": {"default": 0, "attached_pic": 0}
                },
                {
                    "index": 2, "codec_type": "video", "codec_name": "hevc",
                    "width": 1280, "height": 720,
                    "disposition": {"default": 1, "attached_pic": 0}
                }
            ]
        }"#;

        let probe = parse_ffprobe_output(json).expect("valid ffprobe JSON");
        assert_eq!(probe.tracks.video_tracks.len(), 2);
        assert_eq!(probe.tracks.video_tracks[0].id, TrackId(2));
        assert_eq!(probe.tracks.video_tracks[1].id, TrackId(1));
        assert_eq!(probe.tracks.video_tracks[0].width, Some(1280));
    }

    #[test]
    fn missing_duration_and_empty_streams_are_supported() {
        let probe = parse_ffprobe_output(r#"{"format": {}, "streams": []}"#)
            .expect("valid empty media metadata");
        assert_eq!(probe.duration, None);
        assert!(probe.tracks.video_tracks.is_empty());
        assert!(probe.tracks.audio_tracks.is_empty());
        assert!(probe.tracks.subtitle_tracks.is_empty());
        assert_eq!(probe.tracks.active_audio, None);
    }

    #[test]
    fn invalid_json_is_reported() {
        assert!(parse_ffprobe_output("{not json").is_err());
    }
}
