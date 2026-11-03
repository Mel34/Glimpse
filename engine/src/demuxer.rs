//! Demuxer thread handling container parsing, stream discovery,
//! packet queuing, and seek operations.

use std::process::Command;
use std::time::Duration;

use serde_json::Value;
use tracing::info;

use crate::types::{Serial, TrackId, TrackInfo, TrackList, TrackType};

#[derive(Clone)]
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
        self.pts.map(|p| (p as f64 * self.time_base_num as f64) / (self.time_base_den as f64))
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

/// Metadata discovered from the actual media container.
#[derive(Debug, Clone, PartialEq)]
pub struct MediaProbe {
    pub duration: Option<Duration>,
    pub tracks: TrackList,
}

/// Run the system ffprobe utility and return real container metadata.
///
/// The executable is supplied by the system FFmpeg package. This is an
/// intentional first step; packet demuxing will move in-process later.
pub fn probe_media(path: &str) -> Result<MediaProbe, String> {
    info!("Probing media container: {}", path);

    let output = Command::new("ffprobe")
        .args([
            "-v", "error",
            "-show_format",
            "-show_streams",
            "-show_entries",
            "format=duration:stream=index,codec_type,codec_name,width,height,sample_rate,channels:stream_tags=language,title:stream_disposition=default",
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
        let disposition_default = stream
            .get("disposition")
            .and_then(|disposition| disposition.get("default"))
            .and_then(Value::as_u64)
            .unwrap_or(0) != 0;

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

    tracks.active_audio = tracks
        .audio_tracks
        .iter()
        .find(|track| track.is_default)
        .or_else(|| tracks.audio_tracks.first())
        .map(|track| track.id);
    // Subtitles are deliberately disabled until the user selects one.
    tracks.active_subtitle = None;

    Ok(MediaProbe { duration, tracks })
}

/// Compatibility helper for callers that only need the stream list.
pub fn probe_file(path: &str) -> Result<TrackList, String> {
    probe_media(path).map(|probe| probe.tracks)
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
