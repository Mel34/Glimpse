//! Video presentation abstractions and PTS-driven frame scheduling.

use std::sync::Arc;
use std::time::Duration;
use crate::clock::MasterClock;
use crate::types::{DriftReport, Serial};

#[derive(Clone)]
pub struct VideoFrameData {
    pub width: u32,
    pub height: u32,
    pub stride: usize,
    pub rgba_data: Arc<Vec<u8>>,
    pub pts: f64,
    pub duration: f64,
    pub serial: Serial,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ScheduleAction {
    ShowNow,
    Wait(Duration),
    DropFrame,
}

pub struct VideoScheduler {
    pub clock: Arc<MasterClock>,
    pub max_drift_threshold_secs: f64,
    pub total_displayed: u64,
    pub total_dropped: u64,
}

impl VideoScheduler {
    pub fn new(clock: Arc<MasterClock>) -> Self {
        Self {
            clock,
            max_drift_threshold_secs: 0.040,
            total_displayed: 0,
            total_dropped: 0,
        }
    }

    pub fn schedule(&mut self, frame_pts: f64, _frame_duration: f64) -> ScheduleAction {
        let master_pts = self.clock.get_pts();
        let drift = frame_pts - master_pts;

        if self.clock.is_paused() {
            return ScheduleAction::ShowNow;
        }

        if drift > 0.005 {
            ScheduleAction::Wait(Duration::from_secs_f64(drift))
        } else if drift >= -self.max_drift_threshold_secs {
            self.total_displayed += 1;
            ScheduleAction::ShowNow
        } else {
            self.total_dropped += 1;
            ScheduleAction::DropFrame
        }
    }

    pub fn report_drift(&self, frame_pts: f64) -> DriftReport {
        let master = self.clock.get_pts();
        DriftReport {
            master_clock_pts: master,
            video_frame_pts: frame_pts,
            drift_ms: (frame_pts - master) * 1000.0,
            dropped_frames: self.total_dropped,
            displayed_frames: self.total_displayed,
        }
    }
}

pub trait VideoPresenter: Send + Sync {
    fn present_frame(&self, frame: VideoFrameData);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drops_frames_beyond_the_maximum_lateness_even_with_long_frame_duration() {
        let clock = Arc::new(MasterClock::new());
        clock.set_audio_pts(1.0, 0.0);
        clock.resume();
        let mut scheduler = VideoScheduler::new(clock);

        assert_eq!(
            scheduler.schedule(0.95, 1.0),
            ScheduleAction::DropFrame
        );
        assert_eq!(scheduler.total_dropped, 1);
    }

    #[test]
    fn waits_for_frames_more_than_five_milliseconds_early() {
        let clock = Arc::new(MasterClock::new());
        clock.set_audio_pts(1.0, 0.0);
        clock.resume();
        let mut scheduler = VideoScheduler::new(clock);

        assert!(matches!(
            scheduler.schedule(1.02, 0.033),
            ScheduleAction::Wait(delay) if delay > Duration::from_millis(10)
        ));
    }
}
