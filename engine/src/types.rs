//! Common types, events, tracks, and serial tokens for the Glimpse engine.

use std::fmt;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TrackId(pub usize);

impl fmt::Display for TrackId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackType {
    Video,
    Audio,
    Subtitle,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TrackInfo {
    pub id: TrackId,
    pub track_type: TrackType,
    pub title: Option<String>,
    pub language: Option<String>,
    pub codec_name: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub sample_rate: Option<u32>,
    pub channels: Option<u16>,
    pub is_default: bool,
}

impl TrackInfo {
    pub fn display_label(&self) -> String {
        let title_part = self.title.as_deref().unwrap_or("");
        let lang_part = self.language.as_deref().unwrap_or("");
        let mut desc = String::new();

        if !title_part.is_empty() {
            desc.push_str(title_part);
        } else if !lang_part.is_empty() {
            desc.push_str(lang_part);
        } else {
            desc.push_str(&format!("Track {}", self.id.0));
        }

        if !lang_part.is_empty() && !title_part.is_empty() {
            desc.push_str(&format!(" ({})", lang_part));
        }

        if let (Some(w), Some(h)) = (self.width, self.height) {
            desc.push_str(&format!(" - {}x{}", w, h));
        }
        if let Some(sr) = self.sample_rate {
            desc.push_str(&format!(" - {} Hz", sr));
        }

        desc
    }
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct TrackList {
    pub video_tracks: Vec<TrackInfo>,
    pub audio_tracks: Vec<TrackInfo>,
    pub subtitle_tracks: Vec<TrackInfo>,
    pub active_audio: Option<TrackId>,
    pub active_subtitle: Option<TrackId>, // Default is None (OFF)
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Serial(pub u64);

impl Serial {
    pub fn next(&self) -> Self {
        Serial(self.0.wrapping_add(1))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayerState {
    Idle,
    Playing,
    Paused,
    Seeking,
    Ended,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DriftReport {
    pub master_clock_pts: f64,
    pub video_frame_pts: f64,
    pub drift_ms: f64,
    pub dropped_frames: u64,
    pub displayed_frames: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    StateChanged(PlayerState),
    DurationKnown(Duration),
    PositionChanged(Duration),
    TracksChanged(TrackList),
    Drift(DriftReport),
    EndOfStream,
    Error(String),
}