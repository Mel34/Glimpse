//! Glimpse Media Engine
//! Direct in-process FFmpeg (libav*) media engine with audio master clock sync.

pub mod audio;
#[cfg(all(feature = "audio-pipewire", feature = "ffmpeg-system"))]
pub mod audio_playback;
pub mod clock;
pub mod decoder;
pub mod demuxer;
#[cfg(feature = "ffmpeg-system")]
mod ffmpeg_log_filter;
pub mod presenter;
pub mod queue;
pub mod types;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use crossbeam_channel::{bounded, Receiver, Sender};
use parking_lot::Mutex;

#[cfg(all(feature = "audio-pipewire", feature = "ffmpeg-system"))]
use audio_playback::{AudioPlaybackHandle, AudioPlaybackSession};

pub use audio::{AudioSink, MockAudioSink};
#[cfg(feature = "audio-pipewire")]
pub use audio::{PipeWireAudioSink, PipeWireAudioSinkStats};
pub use clock::MasterClock;
pub use presenter::{ScheduleAction, VideoFrameData, VideoScheduler};
pub use types::{DriftReport, Event, PlayerState, Serial, TrackId, TrackInfo, TrackList, TrackType};

pub struct Player {
    clock: Arc<MasterClock>,
    state: Arc<Mutex<PlayerState>>,
    duration: Arc<Mutex<Option<Duration>>>,
    tracks: Arc<Mutex<TrackList>>,
    volume: Arc<Mutex<f32>>,
    audio_enabled: AtomicBool,
    current_serial: Arc<Mutex<Serial>>,
    event_sender: Sender<Event>,
    #[cfg(all(feature = "audio-pipewire", feature = "ffmpeg-system"))]
    path: String,
    seek_position: Arc<Mutex<Duration>>,
    #[cfg(all(feature = "audio-pipewire", feature = "ffmpeg-system"))]
    audio_session: Mutex<Option<AudioPlaybackHandle>>,
    #[allow(dead_code)]
    running: Arc<AtomicBool>,
}

impl Player {
    pub fn open(path_or_uri: &str) -> Result<(Player, Receiver<Event>), String> {
        #[cfg(feature = "ffmpeg-system")]
        ffmpeg_log_filter::install();
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
        let _ = event_tx.send(Event::TracksChanged(discovered_tracks.clone()));
        let _ = event_tx.send(Event::StateChanged(PlayerState::Paused));

        Ok((Player {
            clock,
            state,
            duration,
            tracks,
            volume,
            audio_enabled: AtomicBool::new(discovered_tracks.active_audio.is_some()),
            current_serial,
            event_sender: event_tx,
            #[cfg(all(feature = "audio-pipewire", feature = "ffmpeg-system"))]
            path: path_or_uri.to_owned(),
            seek_position: Arc::new(Mutex::new(Duration::ZERO)),
            #[cfg(all(feature = "audio-pipewire", feature = "ffmpeg-system"))]
            audio_session: Mutex::new(None),
            running,
        }, event_rx))
    }

    pub fn play(&self) {
        let mut state = self.state.lock();
        *state = PlayerState::Playing;

        #[cfg(not(all(feature = "audio-pipewire", feature = "ffmpeg-system")))]
        self.clock.resume();

        #[cfg(all(feature = "audio-pipewire", feature = "ffmpeg-system"))]
        {
            let has_audio = !self.tracks.lock().audio_tracks.is_empty();
            if !has_audio {
                // Video-only media uses the monotonic clock as its playback clock.
                self.clock.resume();
                let _ = self.event_sender.send(Event::StateChanged(PlayerState::Playing));
            } else {
                let initial_volume = if self.audio_enabled.load(Ordering::Acquire) {
                    self.volume()
                } else {
                    0.0
                };
                let mut start_position = *self.seek_position.lock();
                let selected_audio = self.tracks.lock().active_audio.map(|track| track.0);
                let mut reset_position = false;
                let mut session = self.audio_session.lock();

                if session.as_ref().is_some_and(AudioPlaybackHandle::is_finished) {
                    let reached_eof = session
                        .as_ref()
                        .is_some_and(AudioPlaybackHandle::reached_end_of_stream);
                    session.take();
                    if reached_eof {
                        start_position = Duration::ZERO;
                        reset_position = true;
                        self.clock.reset(0.0);
                    }
                }

                let _ = self.event_sender.send(Event::StateChanged(PlayerState::Playing));
                if session.is_none() {
                    let playback = AudioPlaybackSession::new(Arc::clone(&self.clock));
                    match playback.spawn_from(
                        self.path.clone(),
                        initial_volume,
                        start_position,
                        selected_audio,
                        self.event_sender.clone(),
                    ) {
                        Ok(handle) => *session = Some(handle),
                        Err(error) => {
                            *state = PlayerState::Paused;
                            self.clock.pause();
                            let _ = self.event_sender.send(Event::StateChanged(PlayerState::Paused));
                            let _ = self.event_sender.send(Event::Error(error));
                        }
                    }
                } else if let Some(handle) = session.as_ref() {
                    handle.resume();
                }

                drop(session);
                if reset_position {
                    *self.seek_position.lock() = Duration::ZERO;
                }
            }
        }

        #[cfg(not(all(feature = "audio-pipewire", feature = "ffmpeg-system")))]
        let _ = self.event_sender.send(Event::StateChanged(PlayerState::Playing));
    }

    /// Stops the active audio session and resets the playback position.
    pub fn stop(&self) {
        let mut state = self.state.lock();
        *self.seek_position.lock() = Duration::ZERO;
        #[cfg(all(feature = "audio-pipewire", feature = "ffmpeg-system"))]
        {
            let handle = self.audio_session.lock().take();
            if let Some(mut handle) = handle {
                handle.stop();
            }
        }
        self.clock.pause();
        self.clock.reset(0.0);
        *state = PlayerState::Paused;
        let _ = self.event_sender.send(Event::StateChanged(PlayerState::Paused));
    }

    pub fn pause(&self) {
        let mut state = self.state.lock();
        *state = PlayerState::Paused;
        self.clock.pause();
        #[cfg(all(feature = "audio-pipewire", feature = "ffmpeg-system"))]
        if let Some(handle) = self.audio_session.lock().as_ref() {
            handle.pause();
        }
        let _ = self.event_sender.send(Event::StateChanged(PlayerState::Paused));
    }

    pub fn seek(&self, to: Duration) {
        let mut state = self.state.lock();
        let duration = *self.duration.lock();
        let clamped = clamp_seek_target(to, duration);
        let was_playing = *state == PlayerState::Playing;

        // Seeking to or beyond EOF is a normal end-of-playback request. Do
        // not start a replacement audio session: there are no PCM samples
        // after the duration, so prebuffering would incorrectly report an error.
        if duration.is_some_and(|duration| clamped >= duration) {
            *self.seek_position.lock() = clamped;
            #[cfg(all(feature = "audio-pipewire", feature = "ffmpeg-system"))]
            if let Some(mut session) = self.audio_session.lock().take() {
                session.stop();
            }
            self.clock.pause();
            self.clock.reset(clamped.as_secs_f64());
            *state = PlayerState::Ended;

            let mut serial = self.current_serial.lock();
            *serial = serial.next();
            let _ = self.event_sender.send(Event::PositionChanged(clamped));
            let _ = self.event_sender.send(Event::StateChanged(PlayerState::Ended));
            let _ = self.event_sender.send(Event::EndOfStream);
            return;
        }

        *self.seek_position.lock() = clamped;
        // Freeze wall-clock extrapolation while the old audio worker is
        // stopped and the replacement session probes and prebuffers.
        self.clock.pause();
        self.clock.reset(clamped.as_secs_f64());

        #[cfg(all(feature = "audio-pipewire", feature = "ffmpeg-system"))]
        {
            let initial_volume = if self.audio_enabled.load(Ordering::Acquire) { self.volume() } else { 0.0 };
            let mut session = self.audio_session.lock();
            if let Some(mut previous) = session.take() {
                let restart = was_playing && !previous.is_finished();
                previous.stop();
                // The retiring worker may have published one final audio-clock
                // update while shutting down; restore the requested target now.
                self.clock.reset(clamped.as_secs_f64());
                if restart {
                    let playback = AudioPlaybackSession::new(Arc::clone(&self.clock));
                    match playback.spawn_from(
                        self.path.clone(),
                        initial_volume,
                        clamped,
                        self.tracks.lock().active_audio.map(|track| track.0),
                        self.event_sender.clone(),
                    ) {
                        Ok(handle) => *session = Some(handle),
                        Err(error) => {
                            *state = PlayerState::Paused;
                            self.clock.pause();
                            let _ = self.event_sender.send(Event::StateChanged(PlayerState::Paused));
                            let _ = self.event_sender.send(Event::Error(error));
                        }
                    }
                } else if *state == PlayerState::Playing || *state == PlayerState::Ended {
                    *state = PlayerState::Paused;
                    self.clock.pause();
                    let _ = self.event_sender.send(Event::StateChanged(PlayerState::Paused));
                }
            }
        }

        #[cfg(all(feature = "audio-pipewire", feature = "ffmpeg-system"))]
        if was_playing && self.tracks.lock().audio_tracks.is_empty() {
            // Video-only playback has no audio session to resume the clock.
            self.clock.resume();
        }

        #[cfg(not(all(feature = "audio-pipewire", feature = "ffmpeg-system")))]
        {
            if *state == PlayerState::Ended {
                *state = PlayerState::Paused;
                let _ = self.event_sender.send(Event::StateChanged(PlayerState::Paused));
            } else if was_playing {
                self.clock.resume();
            }
        }

        let mut serial = self.current_serial.lock();
        *serial = serial.next();
        let _ = self.event_sender.send(Event::PositionChanged(clamped));
    }

    pub fn set_volume(&self, volume: f32) {
        let clamped = volume.clamp(0.0, 1.0);
        {
            let mut v = self.volume.lock();
            *v = clamped;
        }
        #[cfg(all(feature = "audio-pipewire", feature = "ffmpeg-system"))]
        if let Some(handle) = self.audio_session.lock().as_ref() {
            let effective = if self.audio_enabled.load(Ordering::Acquire) { clamped } else { 0.0 };
            handle.set_volume(effective);
        }
    }

    pub fn volume(&self) -> f32 {
        *self.volume.lock()
    }

    pub fn tracks(&self) -> TrackList {
        self.tracks.lock().clone()
    }

    pub fn select_audio(&self, id: Option<TrackId>) {
        let updated_tracks = {
            let mut tracks = self.tracks.lock();
            if id.is_some_and(|selected| !tracks.audio_tracks.iter().any(|track| track.id == selected)) {
                return;
            }
            if tracks.active_audio == id {
                return;
            }
            tracks.active_audio = id;
            tracks.clone()
        };

        self.audio_enabled.store(id.is_some(), Ordering::Release);

        #[cfg(all(feature = "audio-pipewire", feature = "ffmpeg-system"))]
        if let Some(selected_track) = id {
            let current_state = self.state();
            let was_playing = current_state == PlayerState::Playing;
            let mut state = self.state.lock();
            let position = if current_state == PlayerState::Ended {
                *state = PlayerState::Paused;
                let _ = self.event_sender.send(Event::StateChanged(PlayerState::Paused));
                Duration::ZERO
            } else {
                Duration::from_secs_f64(self.clock.get_pts())
            };
            *self.seek_position.lock() = position;

            let mut session = self.audio_session.lock();
            if let Some(mut previous) = session.take() {
                previous.stop();
            }
            self.clock.reset(position.as_secs_f64());

            if was_playing {
                let playback = AudioPlaybackSession::new(Arc::clone(&self.clock));
                match playback.spawn_from(
                    self.path.clone(),
                    self.volume(),
                    position,
                    Some(selected_track.0),
                    self.event_sender.clone(),
                ) {
                    Ok(handle) => *session = Some(handle),
                    Err(error) => {
                        *state = PlayerState::Paused;
                        self.clock.pause();
                        let _ = self.event_sender.send(Event::StateChanged(PlayerState::Paused));
                        let _ = self.event_sender.send(Event::Error(error));
                    }
                }
            } else {
                self.clock.pause();
            }
        } else {
            if let Some(handle) = self.audio_session.lock().as_ref() {
                handle.set_volume(0.0);
            }
        }

        let _ = self.event_sender.send(Event::TracksChanged(updated_tracks));
    }

    pub fn select_subtitle(&self, id: Option<TrackId>) {
        let updated_tracks = {
            let mut tracks = self.tracks.lock();
            if id.is_some_and(|selected| !tracks.subtitle_tracks.iter().any(|track| track.id == selected)) {
                return;
            }
            if tracks.active_subtitle == id {
                return;
            }
            tracks.active_subtitle = id; // Subtitles remain OFF until explicitly selected.
            tracks.clone()
        };
        let _ = self.event_sender.send(Event::TracksChanged(updated_tracks));
    }

    /// Returns the shared audio master clock used to schedule video frames.
    pub fn master_clock(&self) -> Arc<MasterClock> {
        Arc::clone(&self.clock)
    }

    pub fn position(&self) -> Duration {
        Duration::from_secs_f64(self.clock.get_pts())
    }

    pub fn duration(&self) -> Option<Duration> {
        *self.duration.lock()
    }

    pub fn state(&self) -> PlayerState {
        #[cfg(all(feature = "audio-pipewire", feature = "ffmpeg-system"))]
        let mut state = self.state.lock();
        #[cfg(not(all(feature = "audio-pipewire", feature = "ffmpeg-system")))]
        let state = self.state.lock();
        #[cfg(all(feature = "audio-pipewire", feature = "ffmpeg-system"))]
        if let Some(handle) = self.audio_session.lock().as_ref() {
            if handle.is_finished() {
                *state = if handle.reached_end_of_stream() {
                    PlayerState::Ended
                } else {
                    PlayerState::Paused
                };
            }
        }
        *state
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

    fn test_player(duration: Option<Duration>) -> (Player, Receiver<Event>) {
        let (event_sender, event_receiver) = bounded(16);
        let tracks = TrackList::default();
        (
            Player {
                clock: Arc::new(MasterClock::new()),
                state: Arc::new(Mutex::new(PlayerState::Paused)),
                duration: Arc::new(Mutex::new(duration)),
                tracks: Arc::new(Mutex::new(tracks)),
                volume: Arc::new(Mutex::new(1.0)),
                audio_enabled: AtomicBool::new(false),
                current_serial: Arc::new(Mutex::new(Serial(0))),
                event_sender,
                #[cfg(all(feature = "audio-pipewire", feature = "ffmpeg-system"))]
                path: String::new(),
                seek_position: Arc::new(Mutex::new(Duration::ZERO)),
                #[cfg(all(feature = "audio-pipewire", feature = "ffmpeg-system"))]
                audio_session: Mutex::new(None),
                running: Arc::new(AtomicBool::new(true)),
            },
            event_receiver,
        )
    }

    #[test]
    fn video_only_player_pause_seek_resume_and_stop_lifecycle() {
        let (player, _events) = test_player(Some(Duration::from_secs(30)));

        player.play();
        assert_eq!(player.state(), PlayerState::Playing);
        assert!(!player.master_clock().is_paused());
        std::thread::sleep(Duration::from_millis(15));
        assert!(player.position() > Duration::ZERO);

        player.pause();
        let paused_position = player.position();
        std::thread::sleep(Duration::from_millis(15));
        assert_eq!(player.position(), paused_position);

        player.seek(Duration::from_secs(3));
        assert_eq!(player.state(), PlayerState::Paused);
        assert_eq!(player.position(), Duration::from_secs(3));
        assert_eq!(*player.current_serial.lock(), Serial(1));

        player.play();
        std::thread::sleep(Duration::from_millis(15));
        assert!(player.position() > Duration::from_secs(3));

        player.stop();
        assert_eq!(player.state(), PlayerState::Paused);
        assert_eq!(player.position(), Duration::ZERO);
    }

    #[test]
    fn active_video_only_seek_keeps_playback_running_from_target() {
        let (player, _events) = test_player(Some(Duration::from_secs(30)));
        player.play();
        std::thread::sleep(Duration::from_millis(10));

        player.seek(Duration::from_secs(7));

        assert_eq!(player.state(), PlayerState::Playing);
        assert!(!player.master_clock().is_paused());
        assert_eq!(*player.current_serial.lock(), Serial(1));
        std::thread::sleep(Duration::from_millis(15));
        assert!(player.position() > Duration::from_secs(7));
    }
}
