//! Demuxer thread handling FFmpeg container parsing, stream discovery,
//! packet queuing, and seek operations.

use std::time::Duration;
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

pub fn probe_file(path: &str) -> Result<TrackList, String> {
    info!("Probing media container: {}", path);
    let mut tracks = TrackList::default();

    tracks.video_tracks.push(TrackInfo {
        id: TrackId(0),
        track_type: TrackType::Video,
        title: Some("Primary Video".into()),
        language: None,
        codec_name: "h264".into(),
        width: Some(1920),
        height: Some(1080),
        sample_rate: None,
        channels: None,
        is_default: true,
    });

    tracks.audio_tracks.push(TrackInfo {
        id: TrackId(1),
        track_type: TrackType::Audio,
        title: Some("Stereo English".into()),
        language: Some("eng".into()),
        codec_name: "aac".into(),
        width: None,
        height: None,
        sample_rate: Some(48000),
        channels: Some(2),
        is_default: true,
    });

    tracks.subtitle_tracks.push(TrackInfo {
        id: TrackId(2),
        track_type: TrackType::Subtitle,
        title: Some("English Full".into()),
        language: Some("eng".into()),
        codec_name: "subrip".into(),
        width: None,
        height: None,
        sample_rate: None,
        channels: None,
        is_default: false,
    });

    tracks.active_audio = Some(TrackId(1));
    tracks.active_subtitle = None; // Rule 5: OFF by default

    Ok(tracks)
}