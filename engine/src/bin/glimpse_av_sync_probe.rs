//! Measures PTS-driven video scheduling against the live PipeWire audio clock.
//!
//! This is a timing diagnostic, not a renderer: decoded video frames are
//! scheduled and their audio-master drift is recorded without displaying them.

use std::env;
use std::process::ExitCode;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{bounded, Receiver};
use glimpse_engine::audio_playback::AudioPlaybackSession;
use glimpse_engine::decoder::{VideoDecoder, VideoFrameData, VideoFrameQueue};
use glimpse_engine::demuxer::{self, PacketQueues};
use glimpse_engine::{Event, MasterClock, ScheduleAction, Serial, VideoScheduler};

const PACKET_QUEUE_BYTES: usize = 64 * 1024 * 1024;
const FRAME_QUEUE_BYTES: usize = 256 * 1024 * 1024;
const AUDIO_CLOCK_START_TIMEOUT: Duration = Duration::from_secs(8);
const MAX_PAUSED_CLOCK_DRIFT_SECS: f64 = 0.040;

fn main() -> ExitCode {
    let mut args = env::args().skip(1);
    let Some(path) = args.next() else {
        eprintln!("Usage: glimpse_av_sync_probe <media-file>");
        return ExitCode::from(2);
    };
    if args.next().is_some() {
        eprintln!("Usage: glimpse_av_sync_probe <media-file>");
        return ExitCode::from(2);
    }

    match run(&path) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("A/V sync probe failed: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(path: &str) -> Result<(), String> {
    let probe = demuxer::probe_media(path)?;
    let video_track = probe
        .tracks
        .video_tracks
        .iter()
        .find(|track| track.is_default)
        .or_else(|| probe.tracks.video_tracks.first())
        .ok_or_else(|| "The media file has no video stream".to_string())?;
    if probe.tracks.audio_tracks.is_empty() {
        return Err("The media file has no audio stream".to_string());
    }

    let clock = Arc::new(MasterClock::new());
    let (event_sender, events) = bounded(64);
    let serial = Serial(0);
    let queues = PacketQueues::new(128, PACKET_QUEUE_BYTES);
    let frame_queue = Arc::new(VideoFrameQueue::new(16, FRAME_QUEUE_BYTES));
    let mut decoder = VideoDecoder::open_ffmpeg(
        path,
        video_track.id.0,
        Arc::clone(&queues.video),
        Arc::clone(&frame_queue),
        Arc::clone(&clock),
    )?;

    let worker_path = path.to_owned();
    let worker_tracks = probe.tracks.clone();
    let worker_queues = queues.clone();
    let mut demux_worker = Some(
        thread::Builder::new()
            .name("glimpse-av-sync-demux".to_string())
            .spawn(move || demuxer::demux_file(
                &worker_path,
                &worker_tracks,
                &worker_queues,
                serial,
            ))
            .map_err(|error| format!("Could not start video demux worker: {error}"))?,
    );

    // Start demuxing and prepare the video path before audio begins advancing
    // the master clock, so opening the video decoder cannot make the first
    // frames appear artificially late.
    let audio_session = AudioPlaybackSession::new(Arc::clone(&clock));
    let audio_handle = match audio_session.spawn(path.to_owned(), 1.0, event_sender) {
        Ok(handle) => handle,
        Err(error) => {
            queues.abort();
            if let Some(worker) = demux_worker.take() {
                let _ = worker.join();
            }
            return Err(error);
        }
    };
    if let Err(error) = wait_for_audio_clock(&clock, &audio_handle, &events) {
        queues.abort();
        if let Some(worker) = demux_worker.take() {
            let _ = worker.join();
        }
        return Err(error);
    }

    let mut scheduler = VideoScheduler::new(Arc::clone(&clock));
    let mut displayed_drift_ms = Vec::new();
    let mut dropped_lateness_ms = Vec::new();
    let mut decoded_frames = 0u64;

    loop {
        if let Some(error) = take_audio_error(&events) {
            queues.abort();
            if let Some(worker) = demux_worker.take() {
                let _ = worker.join();
            }
            return Err(format!("Audio playback failed: {error}"));
        }

        if let Err(error) = decoder.decode_step(serial) {
            queues.abort();
            if let Some(worker) = demux_worker.take() {
                let _ = worker.join();
            }
            return Err(error);
        }

        while let Some(frame) = frame_queue.pop_timeout(Duration::ZERO) {
            decoded_frames += 1;
            let (drift_ms, dropped) = match schedule_frame(&mut scheduler, &clock, frame) {
                Ok(result) => result,
                Err(error) => {
                    queues.abort();
                    if let Some(worker) = demux_worker.take() {
                        let _ = worker.join();
                    }
                    return Err(error);
                }
            };
            if dropped {
                dropped_lateness_ms.push((-drift_ms).max(0.0));
            } else {
                displayed_drift_ms.push(drift_ms);
            }
        }

        // Decode video only here; drain unrelated streams so the demux worker
        // cannot block on their bounded packet queues.
        while queues.audio.pop_timeout(Duration::ZERO).is_some() {}
        while queues.subtitle.pop_timeout(Duration::ZERO).is_some() {}

        if demux_worker.as_ref().is_some_and(thread::JoinHandle::is_finished)
            && queues.video.stats().0 == 0
        {
            break;
        }
    }

    let demux_stats = demux_worker
        .take()
        .ok_or_else(|| "Video demux worker was already joined".to_string())?
        .join()
        .map_err(|_| "Video demux worker panicked".to_string())?
        .map_err(|error| format!("Video demux failed: {error}"))?;

    decoder.finish(serial)?;
    while let Some(frame) = frame_queue.pop_timeout(Duration::ZERO) {
        decoded_frames += 1;
        let (drift_ms, dropped) = schedule_frame(&mut scheduler, &clock, frame)?;
        if dropped {
            dropped_lateness_ms.push((-drift_ms).max(0.0));
        } else {
            displayed_drift_ms.push(drift_ms);
        }
    }

    if decoded_frames == 0 {
        return Err("The video stream produced no decoded frames".to_string());
    }

    wait_for_audio_finish(
        &audio_handle,
        &events,
        probe.duration.unwrap_or(Duration::from_secs(600)) + Duration::from_secs(10),
    )?;

    let mut absolute_displayed_drifts: Vec<f64> =
        displayed_drift_ms.iter().map(|value| value.abs()).collect();
    absolute_displayed_drifts.sort_by(f64::total_cmp);
    dropped_lateness_ms.sort_by(f64::total_cmp);
    let max_early_ms = displayed_drift_ms.iter().copied().fold(0.0f64, f64::max);
    let max_late_ms = displayed_drift_ms.iter().copied().fold(0.0f64, |max, value| max.max(-value));
    let max_abs_displayed_ms = absolute_displayed_drifts.iter().copied().fold(0.0f64, f64::max);
    let max_dropped_lateness_ms = dropped_lateness_ms.iter().copied().fold(0.0f64, f64::max);

    println!(
        "Demuxed {} video packets ({} bytes across all streams); decoded {} video frames",
        demux_stats.video_packets, demux_stats.bytes_read, decoded_frames
    );
    println!(
        "Video scheduling: {} displayed, {} dropped",
        scheduler.total_displayed, scheduler.total_dropped
    );
    println!(
        "Displayed-frame drift: max_abs={:.3} ms, p95_abs={:.3} ms, p99_abs={:.3} ms, max_early={:.3} ms, max_late={:.3} ms",
        max_abs_displayed_ms,
        percentile(&absolute_displayed_drifts, 0.95),
        percentile(&absolute_displayed_drifts, 0.99),
        max_early_ms,
        max_late_ms
    );
    println!(
        "Dropped-frame lateness: {} frames, max={:.3} ms",
        dropped_lateness_ms.len(),
        max_dropped_lateness_ms
    );
    println!(
        "A/V sync probe: {} frames measured against PipeWire audio clock",
        displayed_drift_ms.len() + dropped_lateness_ms.len()
    );
    Ok(())
}

fn wait_for_audio_clock(
    clock: &MasterClock,
    audio_handle: &glimpse_engine::audio_playback::AudioPlaybackHandle,
    events: &Receiver<Event>,
) -> Result<(), String> {
    let deadline = Instant::now() + AUDIO_CLOCK_START_TIMEOUT;
    while !clock.has_audio_pts() {
        if let Some(error) = take_audio_error(events) {
            return Err(format!("Audio playback failed before clock startup: {error}"));
        }
        if audio_handle.is_finished() {
            return Err("Audio playback ended before publishing its first audio PTS".to_string());
        }
        if Instant::now() >= deadline {
            return Err("Timed out waiting for the PipeWire audio master clock".to_string());
        }
        thread::sleep(Duration::from_millis(2));
    }
    Ok(())
}

fn wait_for_audio_finish(
    audio_handle: &glimpse_engine::audio_playback::AudioPlaybackHandle,
    events: &Receiver<Event>,
    timeout: Duration,
) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    while !audio_handle.is_finished() {
        if let Some(error) = take_audio_error(events) {
            return Err(format!("Audio playback failed: {error}"));
        }
        if Instant::now() >= deadline {
            return Err("Timed out waiting for PipeWire audio playback to finish".to_string());
        }
        thread::sleep(Duration::from_millis(10));
    }
    if let Some(error) = take_audio_error(events) {
        return Err(format!("Audio playback failed: {error}"));
    }
    Ok(())
}

fn take_audio_error(events: &Receiver<Event>) -> Option<String> {
    loop {
        match events.try_recv() {
            Ok(Event::Error(error)) => return Some(error),
            Ok(_) => {}
            Err(_) => return None,
        }
    }
}

fn schedule_frame(
    scheduler: &mut VideoScheduler,
    clock: &MasterClock,
    frame: VideoFrameData,
) -> Result<(f64, bool), String> {
    loop {
        if clock.is_paused() {
            let drift = frame.pts - clock.get_pts();
            if drift.abs() > MAX_PAUSED_CLOCK_DRIFT_SECS {
                return Err(format!(
                    "Audio master clock paused with video frame {:.3}s away from audio position ({:.3}s)",
                    frame.pts, clock.get_pts()
                ));
            }
            return Ok((drift * 1_000.0, false));
        }

        match scheduler.schedule(frame.pts, frame.duration) {
            ScheduleAction::Wait(delay) => {
                thread::sleep(delay.min(Duration::from_millis(5)));
            }
            ScheduleAction::ShowNow => {
                return Ok((scheduler.report_drift(frame.pts).drift_ms, false));
            }
            ScheduleAction::DropFrame => {
                return Ok((scheduler.report_drift(frame.pts).drift_ms, true));
            }
        }
    }
}

fn percentile(sorted_values: &[f64], percentile: f64) -> f64 {
    if sorted_values.is_empty() {
        return 0.0;
    }
    let index = (((sorted_values.len() - 1) as f64) * percentile).ceil() as usize;
    sorted_values[index]
}
