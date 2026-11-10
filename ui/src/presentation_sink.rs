//! GTK video presentation sink.
//!
//! Decoder workers publish decoded frames; this module owns PTS selection,
//! GDK frame-clock scheduling, GPU texture import, and GTK paintable updates.
//! The engine's MasterClock remains the media-time authority (PipeWire-backed
//! for audio playback); GDK's frame clock controls presentation cadence only.

use crate::video_texture::apply_subtitle_font_size;
use crossbeam_channel::{Receiver, TryRecvError};
use glimpse_engine::decoder::SubtitleCue;
use glimpse_engine::{MasterClock, VideoFrameData};
use glib::object::ObjectExt;
use glib::prelude::Cast;
use gtk4::prelude::GtkWindowExt;
use gtk4::prelude::WidgetExtManual;
use gtk4::prelude::WidgetExt;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

// Retained for the debug tick-history dump; derived Debug output isn't counted
// as a field read by Rust's dead-code analysis.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug)]
struct TickSample {
    interval_us: Option<u64>,
    frame_time_us: i64,
    frame_time_delta_us: Option<i64>,
    refresh_interval_us: i64,
    presentation_time_us: i64,
    window_active: Option<bool>,
}

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
    tick_history: VecDeque<TickSample>,
    last_ingest_at: Option<Instant>,
    last_ingest_interval_us: u64,
    max_ingest_interval_us: u64,
    stream_finished: bool,
}

struct SubtitleSchedulerState {
    pending: std::collections::VecDeque<SubtitleCue>,
    active: Option<SubtitleCue>,
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
        picture: gtk4::Picture,
        subtitle_label: gtk4::Label,
        media_clock: Arc<MasterClock>,
        cancelled: Arc<AtomicBool>,
    ) {
        schedule_frames(
            receiver,
            subtitle_receiver,
            picture,
            subtitle_label,
            media_clock,
            cancelled,
        );
    }
}

fn should_present_video(window_active: Option<bool>) -> bool {
    window_active != Some(false)
}

#[cfg(test)]
mod focus_policy_tests {
    use super::should_present_video;

    #[test]
    fn inactive_window_skips_video_presentation() {
        assert!(!should_present_video(Some(false)));
    }

    #[test]
    fn active_or_unavailable_window_state_does_not_block_presentation() {
        assert!(should_present_video(Some(true)));
        assert!(should_present_video(None));
    }
}

fn schedule_frames(
    receiver: Receiver<Result<VideoFrameData, String>>,
    subtitle_receiver: Receiver<Result<SubtitleCue, String>>,
    picture: gtk4::Picture,
    subtitle_label: gtk4::Label,
    media_clock: Arc<MasterClock>,
    cancelled: Arc<AtomicBool>,
) {
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
        tick_history: VecDeque::with_capacity(32),
        last_ingest_at: None,
        last_ingest_interval_us: 0,
        max_ingest_interval_us: 0,
        stream_finished: false,
    }));
    let picture_weak = picture.downgrade();
    let subtitle_weak = subtitle_label.downgrade();
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

        let window_active = picture.root()
            .and_then(|root| root.downcast::<gtk4::Window>().ok())
            .map(|window| window.is_active());
        let mut state = ingest_state.borrow_mut();
        if !should_present_video(window_active) {
            // If focus is lost before the startup frame is presented, don't pin
            // that old frame; resume by selecting the newest frame for current PTS.
            state.present_first_frame = false;
        }
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
                    let frame_pts = frame.pts;
                    state.pending = None;
                    state.dropped_stale_ingest += 1;
                    if video_debug && (state.dropped_stale_ingest == 1 || state.dropped_stale_ingest % 120 == 0) {
                        tracing::warn!(
                            frame_pts,
                            master_pts,
                            lateness_ms = lateness * 1000.0,
                            dropped_stale_ingest = state.dropped_stale_ingest,
                            queued_frames = ingest_receiver.len(),
                            "Dropping stale video frame during independent queue drain"
                        );
                    }
                    continue;
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
        let window_active = picture.root()
            .and_then(|root| root.downcast::<gtk4::Window>().ok())
            .map(|window| window.is_active());

        let tick_now = Instant::now();
        let tick_interval_us = state.borrow().last_gdk_tick_at
            .as_ref()
            .map(|last_tick| last_tick.elapsed().as_micros() as u64);
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
        // Focus is presentation policy, never playback policy. Continue draining
        // decoded frames and subtitle cues, but avoid GTK updates, frame selection,
        // and GPU import while this window is on an inactive workspace.
        if !should_present_video(window_active) {
            state.borrow_mut().present_first_frame = false;
            return glib::ControlFlow::Continue;
        }

        if let Some(cue) = subtitle_state.active.as_ref() {
            subtitle_label.set_markup(&subtitle_cue_markup(cue));
            apply_subtitle_font_size(&subtitle_label, picture.width());
            subtitle_label.set_visible(true);
        } else {
            subtitle_label.set_visible(false);
        }

        let mut state = state.borrow_mut();
        state.last_gdk_tick_at = Some(tick_now);
        state.last_frame_time_us = Some(frame_time_us);
        let sample = TickSample {
            interval_us: tick_interval_us,
            frame_time_us,
            frame_time_delta_us,
            refresh_interval_us,
            presentation_time_us,
            window_active,
        };
        if state.tick_history.len() == 32 {
            state.tick_history.pop_front();
        }
        state.tick_history.push_back(sample);
        state.tick_count += 1;
        if video_debug && tick_interval_us.is_some_and(|interval| interval >= 100_000) {
            tracing::warn!(
                gap_ms = tick_interval_us.unwrap_or_default() as f64 / 1000.0,
                frame_time_us,
                frame_time_delta_us,
                refresh_interval_us,
                presentation_time_us,
                window_active,
                mapped = picture.is_mapped(),
                visible = picture.is_visible(),
                tick_history = ?state.tick_history,
                "Long gap between GDK frame-clock callbacks"
            );
        }
        if video_debug && state.tick_count % 250 == 0 {
            tracing::info!(
                ticks = state.tick_count,
                frame_time_us,
                frame_time_delta_us,
                tick_interval_us,
                presentation_time_us,
                window_active,
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
        } else if !state.present_first_frame {
            let mut candidate = state.pending.take();

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

                let lateness = master_pts - frame.pts;
                if lateness > 0.040 {
                    state.dropped_late_presentation += 1;
                    if video_debug && (state.dropped_late_presentation == 1 || state.dropped_late_presentation % 120 == 0) {
                        tracing::warn!(
                            frame_pts = frame.pts,
                            master_pts,
                            lateness_ms = lateness * 1000.0,
                            dropped_late_presentation = state.dropped_late_presentation,
                            "Dropping late video frame during presentation"
                        );
                    }
                    continue;
                }

                if frame.pts <= master_pts + presentation_window {
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
        picture.set_paintable(Some(&texture));
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
