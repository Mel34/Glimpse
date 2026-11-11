//! GTK video presentation sink.
//!
//! Decoder workers publish decoded frames; this module owns PTS selection,
//! GDK frame-clock scheduling, GPU texture import, and GTK paintable updates.
//! The engine's MasterClock remains the media-time authority (PipeWire-backed
//! for audio playback); GDK's frame clock controls presentation cadence only.

use crate::video_texture::apply_subtitle_font_size;
use crate::rotatable_picture::RotatablePicture;
use crossbeam_channel::{bounded, Receiver, Sender, TryRecvError, TrySendError};
use std::thread::{self, JoinHandle};
use glimpse_engine::decoder::SubtitleCue;
use glimpse_engine::{MasterClock, VideoFrameData};
use glib::object::ObjectExt;
use glib::prelude::Cast;
use gtk4::prelude::WidgetExtManual;
use gtk4::prelude::WidgetExt;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

struct FrameSchedulerState {
    pending: Option<VideoFrameData>,
    present_first_frame: bool,
    frames_received: u64,
    displayed_frames: u64,
    dropped_stale_ingest: u64,
    dropped_late_presentation: u64,
    dropped_superseded: u64,
    presentation_failures: u64,
    ingest_tick_count: u64,
    tick_count: u64,
    last_gdk_tick_at: Option<Instant>,
    last_frame_time_us: Option<i64>,
    last_ingest_at: Option<Instant>,
    last_ingest_interval_us: u64,
    max_ingest_interval_us: u64,
    stream_finished: bool,
}

struct SubtitleSchedulerState {
    pending: std::collections::VecDeque<SubtitleCue>,
    active: Option<SubtitleCue>,
}

/// Keeps GDK's update phase running while video playback is active.
/// The tick callback owns this guard so the updating request is balanced
/// automatically when the callback is removed.
struct FrameClockUpdateGuard(gdk4::FrameClock);

impl Drop for FrameClockUpdateGuard {
    fn drop(&mut self) {
        self.0.end_updating();
    }
}

fn subtitle_cue_markup(cue: &SubtitleCue) -> String {
    let mut markup = String::new();
    for span in &cue.spans {
        let escaped = glib::markup_escape_text(&span.text);
        if span.bold {
            markup.push_str("<b>");
        } else {
            // ASS's default \b0 must override the overlay's bold CSS style.
            markup.push_str("<span weight=\"normal\">");
        }
        if span.italic { markup.push_str("<i>"); }
        if span.underline { markup.push_str("<u>"); }
        if span.strikethrough { markup.push_str("<s>"); }
        markup.push_str(escaped.as_str());
        if span.strikethrough { markup.push_str("</s>"); }
        if span.underline { markup.push_str("</u>"); }
        if span.italic { markup.push_str("</i>"); }
        if span.bold {
            markup.push_str("</b>");
        } else {
            markup.push_str("</span>");
        }
    }
    markup
}

#[cfg(test)]
mod subtitle_markup_tests {
    use super::subtitle_cue_markup;
    use glimpse_engine::decoder::{SubtitleCue, SubtitleSpan};

    #[test]
    fn renders_all_inline_styles_as_pango_markup() {
        let cue = SubtitleCue {
            start: 0.0,
            end: 1.0,
            text: "formatted".to_string(),
            spans: vec![SubtitleSpan {
                text: "formatted".to_string(),
                bold: true,
                italic: true,
                underline: true,
                strikethrough: true,
            }],
        };
        assert_eq!(
            subtitle_cue_markup(&cue),
            "<b><i><u><s>formatted</s></u></i></b>"
        );
    }

    #[test]
    fn escapes_markup_characters_in_subtitle_text() {
        let cue = SubtitleCue {
            start: 0.0,
            end: 1.0,
            text: "<unsafe & text>".to_string(),
            spans: vec![SubtitleSpan {
                text: "<unsafe & text>".to_string(),
                bold: false,
                italic: false,
                underline: false,
                strikethrough: false,
            }],
        };
        assert_eq!(
            subtitle_cue_markup(&cue),
            "<span weight=\"normal\">&lt;unsafe &amp; text&gt;</span>"
        );
    }
}


/// A GTK-backed sink for decoded video frames.
pub struct GtkPresentationSink;

impl GtkPresentationSink {
    pub fn start(
        receiver: Receiver<Result<VideoFrameData, String>>,
        subtitle_receiver: Receiver<Result<SubtitleCue, String>>,
        picture: RotatablePicture,
        subtitle_label: gtk4::Label,
        media_clock: Arc<MasterClock>,
        cancelled: Arc<AtomicBool>,
        first_frame_ready: Option<Sender<()>>,
        first_frame_pending: Arc<AtomicBool>,
        drop_stale_until_current: Arc<AtomicBool>,
    ) -> Result<JoinHandle<()>, String> {
        let (presentation_sender, presentation_receiver) =
            bounded::<Result<VideoFrameData, String>>(1);
        let presentation_drop_receiver = presentation_receiver.clone();
        let worker_clock = Arc::clone(&media_clock);
        let worker_cancelled = Arc::clone(&cancelled);
        let worker_first_frame_pending = Arc::clone(&first_frame_pending);
        let worker_drop_stale_until_current = Arc::clone(&drop_stale_until_current);
        let worker = thread::Builder::new()
            .name("glimpse-video-ingest".to_string())
            .spawn(move || {
                forward_frames(
                    receiver,
                    presentation_sender,
                    presentation_drop_receiver,
                    worker_clock,
                    worker_cancelled,
                    worker_first_frame_pending,
                    worker_drop_stale_until_current,
                );
            })
            .map_err(|error| format!("Could not start video ingestion worker: {error}"))?;

        schedule_frames(
            presentation_receiver,
            subtitle_receiver,
            picture,
            subtitle_label,
            media_clock,
            cancelled,
            first_frame_ready,
            first_frame_pending,
            drop_stale_until_current,
        );
        Ok(worker)
    }
}

fn forward_frames(
    receiver: Receiver<Result<VideoFrameData, String>>,
    sender: Sender<Result<VideoFrameData, String>>,
    drop_receiver: Receiver<Result<VideoFrameData, String>>,
    media_clock: Arc<MasterClock>,
    cancelled: Arc<AtomicBool>,
    first_frame_pending: Arc<AtomicBool>,
    drop_stale_until_current: Arc<AtomicBool>,
) {
    let video_debug = std::env::var_os("GLIMPSE_VIDEO_DEBUG").is_some();
    let mut forwarded = 0_u64;
    let mut dropped_stale = 0_u64;
    let mut last_heartbeat = Instant::now();

    while !cancelled.load(Ordering::Acquire) {
        let message = match receiver.recv_timeout(Duration::from_millis(5)) {
            Ok(message) => message,
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                if video_debug && last_heartbeat.elapsed() >= Duration::from_secs(1) {
                    tracing::info!(
                        forwarded,
                        dropped_stale,
                        source_queued = receiver.len(),
                        presentation_queued = drop_receiver.len(),
                        master_pts = media_clock.get_pts(),
                        "Video ingestion worker heartbeat"
                    );
                    last_heartbeat = Instant::now();
                }
                continue;
            }
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        };

        let mut message = message;
        loop {
            if cancelled.load(Ordering::Acquire) {
                return;
            }
            match sender.try_send(message) {
                Ok(()) => {
                    forwarded += 1;
                    break;
                }
                Err(TrySendError::Disconnected(_)) => return,
                Err(TrySendError::Full(returned)) => {
                    message = returned;
                    if first_frame_pending.load(Ordering::Acquire) {
                        thread::sleep(Duration::from_millis(5));
                        continue;
                    }

                    match drop_receiver.try_recv() {
                        Ok(Ok(queued_frame)) => {
                            let master_pts = media_clock.get_pts();
                            let lateness = master_pts - queued_frame.pts;
                            if should_drop_stale_frame(
                                queued_frame.pts,
                                master_pts,
                                drop_stale_until_current.load(Ordering::Acquire),
                            ) || lateness > 0.040 {
                                dropped_stale += 1;
                                if video_debug
                                    && (dropped_stale == 1 || dropped_stale % 120 == 0)
                                {
                                    tracing::warn!(
                                        frame_pts = queued_frame.pts,
                                        master_pts,
                                        lateness_ms = lateness * 1000.0,
                                        dropped_stale,
                                        "Ingestion worker discarded stale queued video frame"
                                    );
                                }
                            } else {
                                let _ = sender.try_send(Ok(queued_frame));
                                thread::sleep(Duration::from_millis(5));
                            }
                        }
                        Ok(Err(error)) => {
                            let _ = sender.try_send(Err(error));
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(TryRecvError::Empty) => thread::sleep(Duration::from_millis(2)),
                        Err(TryRecvError::Disconnected) => return,
                    }
                }
            }
        }

        if video_debug && last_heartbeat.elapsed() >= Duration::from_secs(1) {
            tracing::info!(
                forwarded,
                dropped_stale,
                source_queued = receiver.len(),
                presentation_queued = drop_receiver.len(),
                master_pts = media_clock.get_pts(),
                "Video ingestion worker heartbeat"
            );
            last_heartbeat = Instant::now();
        }
    }
}

fn schedule_frames(
    receiver: Receiver<Result<VideoFrameData, String>>,
    subtitle_receiver: Receiver<Result<SubtitleCue, String>>,
    picture: RotatablePicture,
    subtitle_label: gtk4::Label,
    media_clock: Arc<MasterClock>,
    cancelled: Arc<AtomicBool>,
    first_frame_ready: Option<Sender<()>>,
    first_frame_pending: Arc<AtomicBool>,
    drop_stale_until_current: Arc<AtomicBool>,
) {
    let first_frame_ready = Rc::new(RefCell::new(first_frame_ready));
    // The sink reads engine media time but never adjusts it. PipeWire remains
    // authoritative for audio playback; this callback only chooses a frame
    // that is ready for the current GDK presentation opportunity.
    let video_debug = std::env::var_os("GLIMPSE_VIDEO_DEBUG").is_some();
    let ingest_video_debug = video_debug;
    let state = Rc::new(RefCell::new(FrameSchedulerState {
        pending: None,
        present_first_frame: true,
        frames_received: 0,
        displayed_frames: 0,
        dropped_stale_ingest: 0,
        dropped_late_presentation: 0,
        dropped_superseded: 0,
        presentation_failures: 0,
        ingest_tick_count: 0,
        tick_count: 0,
        last_gdk_tick_at: None,
        last_frame_time_us: None,
        last_ingest_at: None,
        last_ingest_interval_us: 0,
        max_ingest_interval_us: 0,
        stream_finished: false,
    }));
    let picture_weak = picture.downgrade();
    let subtitle_weak = subtitle_label.downgrade();
    let frame_clock_update_guard = if std::env::var_os("GLIMPSE_FORCE_FRAME_CLOCK_UPDATES").is_some() {
        picture.frame_clock().map(|frame_clock| {
            frame_clock.begin_updating();
            tracing::info!(
                mapped = picture.is_mapped(),
                "Experimental mode: keeping GDK frame-clock updates active"
            );
            FrameClockUpdateGuard(frame_clock)
        })
    } else {
        None
    };
    let subtitle_state = RefCell::new(SubtitleSchedulerState {
        pending: std::collections::VecDeque::new(),
        active: None,
    });

    if video_debug {
        tracing::info!(
            mapped = picture.is_mapped(),
            visible = picture.is_visible(),
            "Starting vblank-aware video presentation scheduler"
        );

    }

    // Keep frame ingestion independent of GDK's visibility-aware frame clock.
    // This timer drains stale frames while the window is on another desktop.
    let ingest_state = Rc::clone(&state);
    let ingest_receiver = receiver.clone();
    let ingest_clock = Arc::clone(&media_clock);
    let ingest_cancelled = Arc::clone(&cancelled);
    let ingest_picture = picture.downgrade();
    glib::timeout_add_local(Duration::from_millis(5), move || {
        if ingest_cancelled.load(Ordering::Acquire) {
            return glib::ControlFlow::Break;
        }
        let Some(picture) = ingest_picture.upgrade() else {
            return glib::ControlFlow::Break;
        };

        let mut state = ingest_state.borrow_mut();
        if state.stream_finished {
            return glib::ControlFlow::Break;
        }
        let ingest_now = Instant::now();
        if let Some(last_ingest_at) = state.last_ingest_at.replace(ingest_now) {
            let interval_us = last_ingest_at.elapsed().as_micros() as u64;
            state.last_ingest_interval_us = interval_us;
            state.max_ingest_interval_us = state.max_ingest_interval_us.max(interval_us);
        }
        state.ingest_tick_count += 1;
        if ingest_video_debug && state.ingest_tick_count % 200 == 0 {
            tracing::info!(
                ingest_ticks = state.ingest_tick_count,
                frames_received = state.frames_received,
                displayed_frames = state.displayed_frames,
                dropped_stale_ingest = state.dropped_stale_ingest,
                dropped_late_presentation = state.dropped_late_presentation,
                dropped_superseded = state.dropped_superseded,
                presentation_failures = state.presentation_failures,
                pending_pts = state.pending.as_ref().map(|frame| frame.pts),
                queued_frames = ingest_receiver.len(),
                master_pts = ingest_clock.get_pts(),
                since_last_gdk_tick_ms = state.last_gdk_tick_at
                    .as_ref()
                    .map(|last_tick| last_tick.elapsed().as_secs_f64() * 1000.0),
                last_ingest_interval_us = state.last_ingest_interval_us,
                max_ingest_interval_us = state.max_ingest_interval_us,
                last_frame_time_us = state.last_frame_time_us,
                mapped = picture.is_mapped(),
                visible = picture.is_visible(),
                "Video ingestion heartbeat"
            );
        }

        loop {
            if let Some(frame) = state.pending.as_ref() {
                let master_pts = ingest_clock.get_pts();
                let lateness = master_pts - frame.pts;
                // Never discard the startup frame before GTK has had a chance
                // to present it, even if the audio clock started first.
                if !state.present_first_frame && lateness > 0.040 {
                    // Keep the newest late frame as a recovery fallback. Only
                    // discard it once a newer decoded frame is available.
                    match ingest_receiver.try_recv() {
                        Ok(Ok(frame)) => {
                            let frame_pts = state.pending.take().map(|pending| pending.pts);
                            state.dropped_stale_ingest += 1;
                            if video_debug && (state.dropped_stale_ingest == 1 || state.dropped_stale_ingest % 120 == 0) {
                                tracing::warn!(
                                    frame_pts,
                                    master_pts,
                                    lateness_ms = lateness * 1000.0,
                                    dropped_stale_ingest = state.dropped_stale_ingest,
                                    queued_frames = ingest_receiver.len(),
                                    "Dropping older stale video frame; retaining newer frame"
                                );
                            }
                            state.frames_received += 1;
                            state.pending = Some(frame);
                            continue;
                        }
                        Ok(Err(error)) => {
                            tracing::error!(
                                %error,
                                frames_received = state.frames_received,
                                displayed_frames = state.displayed_frames,
                                dropped_stale_ingest = state.dropped_stale_ingest,
                                dropped_late_presentation = state.dropped_late_presentation,
                                dropped_superseded = state.dropped_superseded,
                                presentation_failures = state.presentation_failures,
                                "Engine video decoder failed; stopping frame ingestion"
                            );
                            state.stream_finished = true;
                            break;
                        }
                        Err(TryRecvError::Empty) => break,
                        Err(TryRecvError::Disconnected) => {
                            state.stream_finished = true;
                            break;
                        }
                    }
                }
                break;
            }

            match ingest_receiver.try_recv() {
                Ok(Ok(frame)) => {
                    state.frames_received += 1;
                    state.pending = Some(frame);
                },
                Ok(Err(error)) => {
                    tracing::error!(
                        %error,
                        frames_received = state.frames_received,
                        displayed_frames = state.displayed_frames,
                        dropped_stale_ingest = state.dropped_stale_ingest,
                        dropped_late_presentation = state.dropped_late_presentation,
                        dropped_superseded = state.dropped_superseded,
                        presentation_failures = state.presentation_failures,
                        "Engine video decoder failed; stopping frame ingestion"
                    );
                    state.stream_finished = true;
                    return glib::ControlFlow::Break;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    state.stream_finished = true;
                    return glib::ControlFlow::Break;
                }
            }
        }

        glib::ControlFlow::Continue
    });

    // GDK invokes this callback in its frame-update phase, immediately before
    // GTK paints. Presentation therefore follows the display's refresh cadence.
    picture.add_tick_callback(move |_, frame_clock| {
        // Keep the guard alive for the lifetime of this tick callback.
        let _keep_frame_clock_updating = &frame_clock_update_guard;
        if cancelled.load(Ordering::Acquire) {
            tracing::debug!("Video frame scheduler stopped: playback run cancelled");
            return glib::ControlFlow::Break;
        }
        let Some(picture) = picture_weak.upgrade() else {
            tracing::warn!("Video frame scheduler stopped: picture widget was destroyed");
            return glib::ControlFlow::Break;
        };
        let Some(subtitle_label) = subtitle_weak.upgrade() else {
            tracing::warn!("Video frame scheduler stopped: subtitle widget was destroyed");
            return glib::ControlFlow::Break;
        };
        let tick_interval_us = {
            let mut state = state.borrow_mut();
            state.last_gdk_tick_at
                .replace(Instant::now())
                .map(|last_tick| last_tick.elapsed().as_micros() as u64)
        };

        let frame_time_us = frame_clock.frame_time();
        let frame_time_delta_us = state.borrow().last_frame_time_us
            .map(|last_time| frame_time_us - last_time);
        let (refresh_interval_us, presentation_time_us) = frame_clock.refresh_info(frame_time_us);
        let refresh_interval_us = if refresh_interval_us > 0 {
            refresh_interval_us
        } else {
            16_667
        };
        let presentation_window = refresh_interval_us as f64 / 1_000_000.0;
        let master_pts = media_clock.get_pts();
        // GDK's predicted presentation timestamp and GLib's monotonic clock
        // share a time domain. Map the remaining monotonic lead onto the
        // current audio-master PTS; never alter the master clock itself.
        let predicted_lead_us = presentation_time_us.saturating_sub(glib::monotonic_time());
        let predicted_lead_secs = if presentation_time_us > 0 {
            predicted_lead_us.max(0) as f64 / 1_000_000.0
        } else {
            // Some backends cannot provide a prediction. Retain the previous
            // one-refresh fallback when no prediction is available at all.
            presentation_window
        };
        // Convert the wall-clock lead until the next display refresh into media
        // time using the same authoritative rate that drives PipeWire. Without
        // this scaling, audio advances (for example) 32× during a refresh while
        // the video scheduler only looks one wall-clock refresh ahead.
        let presentation_deadline_pts = presentation_deadline_pts(
            master_pts,
            predicted_lead_secs,
            media_clock.speed(),
        );

        let mut subtitle_state = subtitle_state.borrow_mut();
        loop {
            match subtitle_receiver.try_recv() {
                Ok(Ok(cue)) => subtitle_state.pending.push_back(cue),
                Ok(Err(error)) => tracing::warn!(%error, "Subtitle decoder failed"),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => break,
            }
        }
        while subtitle_state.pending.front().is_some_and(|cue| cue.end <= master_pts) {
            subtitle_state.pending.pop_front();
        }
        if subtitle_state.active.as_ref().is_some_and(|cue| cue.end <= master_pts) {
            subtitle_state.active = None;
        }
        if subtitle_state.active.is_none() {
            if let Some(cue) = subtitle_state.pending.front() {
                if cue.start <= master_pts {
                    subtitle_state.active = subtitle_state.pending.pop_front();
                }
            }
        }
        // Presentation continues independently of keyboard focus.
        if let Some(cue) = subtitle_state.active.as_ref() {
            subtitle_label.set_markup(&subtitle_cue_markup(cue));
            apply_subtitle_font_size(&subtitle_label, picture.width());
            subtitle_label.set_visible(true);
        } else {
            subtitle_label.set_visible(false);
        }

        let mut state = state.borrow_mut();
        state.last_frame_time_us = Some(frame_time_us);
        state.tick_count += 1;
        if video_debug && state.tick_count % 250 == 0 {
            tracing::info!(
                ticks = state.tick_count,
                frame_time_us,
                frame_time_delta_us,
                tick_interval_us,
                presentation_time_us,
                frames_received = state.frames_received,
                displayed_frames = state.displayed_frames,
                dropped_stale_ingest = state.dropped_stale_ingest,
                dropped_late_presentation = state.dropped_late_presentation,
                dropped_superseded = state.dropped_superseded,
                presentation_failures = state.presentation_failures,
                pending_pts = state.pending.as_ref().map(|frame| frame.pts),
                queued_frames = receiver.len(),
                master_pts,
                refresh_interval_us,
                refresh_hz = 1_000_000.0 / refresh_interval_us as f64,
                mapped = picture.is_mapped(),
                visible = picture.is_visible(),
                "Vblank-aware video presentation heartbeat"
            );
        }

        // Preserve the first decoded frame for the first GDK presentation.
        // During playback, consume frames in PTS order until the first future
        // frame. Select the newest frame ready for this refresh and leave later
        // frames in the channel for subsequent refreshes.
        let mut selected_frame = if state.present_first_frame {
            state.pending.take()
        } else {
            None
        };

        if selected_frame.is_some() {
            state.present_first_frame = false;
            first_frame_pending.store(false, Ordering::Release);
        } else if !state.present_first_frame {
            let mut candidate = state.pending.take();
            let mut latest_late_frame = None;

            loop {
                let frame = if let Some(frame) = candidate.take() {
                    frame
                } else {
                    match receiver.try_recv() {
                        Ok(Ok(frame)) => {
                            state.frames_received += 1;
                            frame
                        },
                        Ok(Err(error)) => {
                            tracing::error!(
                                %error,
                                frames_received = state.frames_received,
                                displayed_frames = state.displayed_frames,
                                dropped_stale_ingest = state.dropped_stale_ingest,
                                dropped_late_presentation = state.dropped_late_presentation,
                                dropped_superseded = state.dropped_superseded,
                                presentation_failures = state.presentation_failures,
                                "Engine video decoder failed during frame selection"
                            );
                            state.stream_finished = true;
                            break;
                        }
                        Err(TryRecvError::Empty) => break,
                        Err(TryRecvError::Disconnected) => {
                            state.stream_finished = true;
                            break;
                        }
                    }
                };

                if should_drop_stale_frame(
                    frame.pts,
                    master_pts,
                    drop_stale_until_current.load(Ordering::Acquire),
                ) {
                    state.dropped_late_presentation += 1;
                    continue;
                }

                let lateness = presentation_deadline_pts - frame.pts;
                if lateness > 0.040 {
                    // Retain the newest late frame as a fallback. If another
                    // late frame arrives, the older fallback is now obsolete.
                    if let Some(replaced_late_frame) = latest_late_frame.replace(frame) {
                        state.dropped_late_presentation += 1;
                        if video_debug && (state.dropped_late_presentation == 1 || state.dropped_late_presentation % 120 == 0) {
                            tracing::warn!(
                                frame_pts = replaced_late_frame.pts,
                                master_pts,
                                dropped_late_presentation = state.dropped_late_presentation,
                                "Dropping older late video frame; retaining newest available frame"
                            );
                        }
                    }
                    continue;
                }

                if frame.pts <= presentation_deadline_pts {
                    // A normally eligible frame is preferable to any late
                    // fallback because it is closer to the audio master clock.
                    if latest_late_frame.take().is_some() {
                        state.dropped_late_presentation += 1;
                    }
                    if let Some(replaced_frame) = selected_frame.replace(frame) {
                        state.dropped_superseded += 1;
                        if state.dropped_superseded == 1 || state.dropped_superseded % 120 == 0 {
                            tracing::debug!(
                                replaced_pts = replaced_frame.pts,
                                master_pts,
                                dropped_superseded = state.dropped_superseded,
                                "Replacing an eligible video frame with a newer frame for this refresh"
                            );
                        }
                    }
                } else {
                    // Frames arrive in presentation order. Keep the first
                    // future frame and leave the rest queued for later ticks.
                    state.pending = Some(frame);
                    break;
                }
            }

            // If no normally eligible frame was found, show the newest late
            // frame rather than leaving the picture frozen on an older image.
            if selected_frame.is_none() {
                selected_frame = latest_late_frame.take();
            } else if latest_late_frame.take().is_some() {
                state.dropped_late_presentation += 1;
            }
        }

        let Some(frame) = selected_frame else {
            if state.stream_finished && state.pending.is_none() {
                subtitle_label.set_text("");
                subtitle_label.set_visible(false);
                if video_debug {
                    tracing::info!(
                        frames_received = state.frames_received,
                        displayed_frames = state.displayed_frames,
                        dropped_stale_ingest = state.dropped_stale_ingest,
                        dropped_late_presentation = state.dropped_late_presentation,
                        dropped_superseded = state.dropped_superseded,
                        presentation_failures = state.presentation_failures,
                        ticks = state.tick_count,
                        "Engine video stream finished; stopping frame scheduler"
                    );
                }
                return glib::ControlFlow::Break;
            }
            return glib::ControlFlow::Continue;
        };

        if drop_stale_until_current.load(Ordering::Acquire)
            && frame.pts + 0.000_001 >= master_pts
        {
            drop_stale_until_current.store(false, Ordering::Release);
        }

        let texture_import_started = Instant::now();
        let texture: gdk4::Texture = match frame.vaapi_frame.as_ref() {
            Some(vaapi_frame) => {
                #[cfg(feature = "vaapi-hdr")]
                {
                    match crate::vaapi_texture::create_texture(vaapi_frame) {
                        Ok(texture) => texture,
                        Err(error) => {
                            state.presentation_failures += 1;
                            tracing::error!(
                                %error,
                                pts = frame.pts,
                                presentation_failures = state.presentation_failures,
                                "Could not import VAAPI frame into GTK"
                            );
                            return glib::ControlFlow::Break;
                        }
                    }
                }
                #[cfg(not(feature = "vaapi-hdr"))]
                {
                    let _ = vaapi_frame;
                    tracing::error!(
                        "A VAAPI frame reached the presenter, but Glimpse was built without the vaapi-hdr feature"
                    );
                    return glib::ControlFlow::Break;
                }
            }
            None => {
                let pixels = Arc::try_unwrap(frame.rgba_data)
                    .unwrap_or_else(|shared| shared.as_ref().clone());
                let bytes = glib::Bytes::from_owned(pixels);
                gdk4::MemoryTexture::new(
                    frame.width as i32,
                    frame.height as i32,
                    gdk4::MemoryFormat::R8g8b8a8,
                    &bytes,
                    frame.stride,
                )
                .upcast::<gdk4::Texture>()
            }
        };
        let texture_import_us = texture_import_started.elapsed().as_micros() as u64;
        let paintable_started = Instant::now();
        picture.set_paintable(Some(texture.upcast_ref()));
        if let Some(ready) = first_frame_ready.borrow_mut().take() {
            let _ = ready.send(());
        }
        let set_paintable_us = paintable_started.elapsed().as_micros() as u64;
        if video_debug && (texture_import_us >= 5_000 || set_paintable_us >= 2_000) {
            tracing::warn!(
                pts = frame.pts,
                texture_import_us,
                set_paintable_us,
                refresh_interval_us,
                frame_time_us,
                "Slow video texture presentation operation"
            );
        }
        state.displayed_frames += 1;
        if video_debug && state.displayed_frames == 1 {
            tracing::info!(
                frame_pts = frame.pts,
                master_pts,
                refresh_interval_us,
                refresh_hz = 1_000_000.0 / refresh_interval_us as f64,
                mapped = picture.is_mapped(),
                visible = picture.is_visible(),
                "Presented first video frame on GDK frame clock"
            );
        }

        glib::ControlFlow::Continue
    });
}

fn should_drop_stale_frame(frame_pts: f64, master_pts: f64, catch_up: bool) -> bool {
    catch_up && frame_pts + 0.000_001 < master_pts
}

/// Map the next display opportunity's wall-clock lead into media time.
/// Audio and video use the same playback rate, so a 16 ms display lead at 32×
/// represents roughly 512 ms of media time, not 16 ms.
fn presentation_deadline_pts(master_pts: f64, wall_lead_secs: f64, playback_speed: f64) -> f64 {
    let speed = if playback_speed.is_finite() {
        playback_speed.clamp(0.5, 32.0)
    } else {
        1.0
    };
    master_pts + wall_lead_secs.max(0.0) * speed
}

#[cfg(test)]
mod stale_frame_policy_tests {
    use super::should_drop_stale_frame;

    #[test]
    fn catch_up_mode_drops_frames_behind_the_media_clock() {
        assert!(should_drop_stale_frame(4.950, 5.000, true));
    }

    #[test]
    fn catch_up_mode_keeps_current_and_future_frames() {
        assert!(!should_drop_stale_frame(5.000, 5.000, true));
        assert!(!should_drop_stale_frame(5.033, 5.000, true));
    }

    #[test]
    fn normal_playback_keeps_the_existing_late_frame_fallback() {
        assert!(!should_drop_stale_frame(4.950, 5.000, false));
    }
}

#[cfg(test)]
mod presentation_deadline_tests {
    use super::presentation_deadline_pts;

    #[test]
    fn scales_display_lead_by_the_shared_playback_rate() {
        let normal = presentation_deadline_pts(10.0, 1.0 / 60.0, 1.0);
        let accelerated = presentation_deadline_pts(10.0, 1.0 / 60.0, 32.0);

        assert!((normal - (10.0 + 1.0 / 60.0)).abs() < 1e-9);
        assert!((accelerated - (10.0 + 32.0 / 60.0)).abs() < 1e-9);
    }

    #[test]
    fn invalid_speed_falls_back_to_normal_rate() {
        assert_eq!(presentation_deadline_pts(10.0, 0.02, f64::NAN), 10.02);
    }

    #[test]
    fn negative_display_lead_does_not_move_deadline_backwards() {
        assert_eq!(presentation_deadline_pts(10.0, -0.02, 4.0), 10.0);
    }
}

