use std::sync::Arc;
use std::time::Duration;

use glimpse_engine::{MasterClock, ScheduleAction, VideoScheduler};

#[test]
fn video_scheduler_tracks_the_audio_master_clock_across_wait_show_and_drop() {
    let clock = Arc::new(MasterClock::new());
    clock.set_audio_pts(10.0, 0.025);
    clock.resume();

    let mut scheduler = VideoScheduler::new(Arc::clone(&clock));

    // Audio device latency moves the effective master clock back to 9.975s.
    assert!(matches!(
        scheduler.schedule(10.100, 0.033),
        ScheduleAction::Wait(delay) if delay > Duration::from_millis(100)
    ));

    clock.set_audio_pts(10.0, 0.025);
    assert_eq!(
        scheduler.schedule(9.960, 0.033),
        ScheduleAction::ShowNow
    );
    clock.set_audio_pts(10.0, 0.025);
    assert_eq!(
        scheduler.schedule(9.900, 0.033),
        ScheduleAction::DropFrame
    );

    let report = scheduler.report_drift(9.0);
    assert!(report.master_clock_pts >= 9.975);
    assert!(report.drift_ms.is_finite());
    assert_eq!(report.displayed_frames, 1);
    assert_eq!(report.dropped_frames, 1);
    assert_eq!(scheduler.total_displayed, 1);
    assert_eq!(scheduler.total_dropped, 1);
}
