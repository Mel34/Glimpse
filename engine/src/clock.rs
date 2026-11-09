//! Audio master clock and fallback monotonic clock implementation.
//! Modeled on fftools/ffplay.c's Clock struct and audio master synchronization.

use std::time::Instant;
use parking_lot::Mutex;

#[derive(Debug)]
pub struct MasterClock {
    inner: Mutex<ClockState>,
}

#[derive(Debug, Clone)]
struct ClockState {
    pts: f64,
    pts_drift: f64,
    last_updated: Instant,
    speed: f64,
    paused: bool,
    has_audio_pts: bool,
}

impl MasterClock {
    pub fn new() -> Self {
        let now = Instant::now();
        Self {
            inner: Mutex::new(ClockState {
                pts: 0.0,
                pts_drift: 0.0,
                last_updated: now,
                speed: 1.0,
                paused: true,
                has_audio_pts: false,
            }),
        }
    }

    pub fn reset(&self, target_pts: f64) {
        let mut state = self.inner.lock();
        state.pts = target_pts.max(0.0);
        state.last_updated = Instant::now();
        state.pts_drift = target_pts;
        state.has_audio_pts = false;
    }

    pub fn pause(&self) {
        let mut state = self.inner.lock();
        if !state.paused {
            state.pts = self.get_pts_locked(&state);
            state.paused = true;
        }
    }

    pub fn resume(&self) {
        let mut state = self.inner.lock();
        if state.paused {
            state.last_updated = Instant::now();
            state.paused = false;
        }
    }

    pub fn set_audio_pts(&self, hardware_pts: f64, device_latency_secs: f64) {
        let mut state = self.inner.lock();
        let effective_pts = (hardware_pts - device_latency_secs).max(0.0);
        state.pts = effective_pts;
        state.last_updated = Instant::now();
        state.has_audio_pts = true;
    }

    pub fn get_pts(&self) -> f64 {
        let state = self.inner.lock();
        self.get_pts_locked(&state)
    }

    fn get_pts_locked(&self, state: &ClockState) -> f64 {
        if state.paused {
            return state.pts;
        }
        let elapsed = state.last_updated.elapsed().as_secs_f64() * state.speed;
        state.pts + elapsed
    }

    pub fn calculate_drift(&self, video_pts: f64) -> f64 {
        let master = self.get_pts();
        video_pts - master
    }

    pub fn has_audio_pts(&self) -> bool {
        self.inner.lock().has_audio_pts
    }

    pub fn is_paused(&self) -> bool {
        self.inner.lock().paused
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_clock_readiness_is_cleared_by_reset() {
        let clock = MasterClock::new();
        assert!(!clock.has_audio_pts());

        clock.set_audio_pts(1.0, 0.0);
        assert!(clock.has_audio_pts());

        clock.reset(2.0);
        assert!(!clock.has_audio_pts());
    }

    #[test]
    fn reset_after_pause_holds_seek_target_until_resumed() {
        let clock = MasterClock::new();
        clock.resume();
        std::thread::sleep(std::time::Duration::from_millis(10));

        clock.pause();
        clock.reset(2.0);
        std::thread::sleep(std::time::Duration::from_millis(20));

        assert!(clock.is_paused());
        assert_eq!(clock.get_pts(), 2.0);

        clock.resume();
        std::thread::sleep(std::time::Duration::from_millis(10));
        assert!(clock.get_pts() > 2.0);
    }
}
