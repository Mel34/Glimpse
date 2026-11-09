//! Glimpse Media Engine
//! Direct in-process FFmpeg (libav*) media engine with audio master clock sync.

pub mod audio;
#[cfg(all(feature = "audio-pipewire", feature = "ffmpeg-system"))]
pub mod audio_playback;
pub mod clock;
pub mod decoder;
pub mod demuxer;
pub mod presenter;
pub mod queue;
pub mod types;

use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;
use crossbeam_channel::{bounded, Receiver, Sender};
use parking_lot::Mutex;

pub use audio::{AudioSink, MockAudioSink};
#[cfg(feature = "audio-pipewire")]
pub use audio::{PipeWireAudioSink, PipeWireAudioSinkStats};
pub use clock::MasterClock;
pub use presenter::{ScheduleAction, VideoFrameData, VideoPresenter, VideoScheduler};
pub use types::{DriftReport, Event, PlayerState, Serial, TrackId, TrackInfo, TrackList, TrackType};

pub struct Player {
    clock: Arc<MasterClock>,
    state: Arc<Mutex<PlayerState>>,
    duration: Arc<Mutex<Option<Duration>>>,
    tracks: Arc<Mutex<TrackList>>,
    volume: Arc<Mutex<f32>>,
    current_serial: Arc<Mutex<Serial>>,
    event_sender: Sender<Event>,
    #[allow(dead_code)]
    running: Arc<AtomicBool>,
}

impl Player {
    pub fn open(path_or_uri: &str) -> Result<(Player, Receiver<Event>), String> {
        let (event_tx, event_rx) = bounded(256);
        let probe = demuxer::probe_media(path_or_uri)?;
        let initial_duration = probe.duration;
        let discovered_tracks = probe.tracks;

        let clock = Arc::new(MasterClock::new());
        let current_serial = Arc::new(Mutex::new(Serial(0)));
        let state = Arc::new(Mutex::new(PlayerState::Paused));
        let duration = Arc::new(Mutex::new(initial_duration));
        let tracks = Arc::new(Mutex::new(discovered_tracks.clone()));
        let volume = Arc::new(Mutex::new(1.0f32));
        let running = Arc::new(AtomicBool::new(true));

        if let Some(dur) = initial_duration {
            let _ = event_tx.send(Event::DurationKnown(dur));
        }
        let _ = event_tx.send(Event::TracksChanged(discovered_tracks));
        let _ = event_tx.send(Event::StateChanged(PlayerState::Paused));

        Ok((Player {
            clock,
            state,
            duration,
            tracks,
            volume,
            current_serial,
            event_sender: event_tx,
            running,
        }, event_rx))
    }

    pub fn play(&self) {
        let mut state = self.state.lock();
        *state = PlayerState::Playing;
        self.clock.resume();
        let _ = self.event_sender.send(Event::StateChanged(PlayerState::Playing));
    }

    pub fn pause(&self) {
        let mut state = self.state.lock();
        *state = PlayerState::Paused;
        self.clock.pause();
        let _ = self.event_sender.send(Event::StateChanged(PlayerState::Paused));
    }

    pub fn seek(&self, to: Duration) {
        let mut serial = self.current_serial.lock();
        *serial = serial.next();

        let clamped = clamp_seek_target(to, *self.duration.lock());
        self.clock.reset(clamped.as_secs_f64());
        let _ = self.event_sender.send(Event::PositionChanged(clamped));
    }

    pub fn set_volume(&self, volume: f32) {
        let clamped = volume.clamp(0.0, 1.0);
        let mut v = self.volume.lock();
        *v = clamped;
    }

    pub fn volume(&self) -> f32 {
        *self.volume.lock()
    }

    pub fn tracks(&self) -> TrackList {
        self.tracks.lock().clone()
    }

    pub fn select_audio(&self, id: Option<TrackId>) {
        let mut t = self.tracks.lock();
        t.active_audio = id;
        let _ = self.event_sender.send(Event::TracksChanged(t.clone()));
    }

    pub fn select_subtitle(&self, id: Option<TrackId>) {
        let mut t = self.tracks.lock();
        t.active_subtitle = id; // Default is None (OFF)
        let _ = self.event_sender.send(Event::TracksChanged(t.clone()));
    }

    pub fn position(&self) -> Duration {
        Duration::from_secs_f64(self.clock.get_pts())
    }

    pub fn duration(&self) -> Option<Duration> {
        *self.duration.lock()
    }

    pub fn state(&self) -> PlayerState {
        *self.state.lock()
    }
}

fn clamp_seek_target(target: Duration, duration: Option<Duration>) -> Duration {
    let maximum = duration.unwrap_or(Duration::from_secs(3600));
    target.min(maximum)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seek_target_is_clamped_to_known_duration() {
        assert_eq!(
            clamp_seek_target(Duration::from_secs(500), Some(Duration::from_secs(124))),
            Duration::from_secs(124)
        );
        assert_eq!(
            clamp_seek_target(Duration::from_secs(45), Some(Duration::from_secs(124))),
            Duration::from_secs(45)
        );
    }

    #[test]
    fn unknown_duration_uses_one_hour_seek_ceiling() {
        assert_eq!(
            clamp_seek_target(Duration::from_secs(5000), None),
            Duration::from_secs(3600)
        );
    }
}
