//! Engine-clock-driven video presentation through GTK4 textures.

use glimpse_engine::{MasterClock, TrackList};
use glib::object::ObjectExt;
use glib::prelude::Cast;
use gtk4::prelude::WidgetExtManual;
use gtk4::prelude::WidgetExt;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use crossbeam_channel::{bounded, unbounded, Receiver, Sender, TryRecvError};
use glimpse_engine::decoder::{decode_external_subtitle_file, SubtitleCue, SubtitleDecoder, VideoDecoder, VideoFrameQueue};
use glimpse_engine::demuxer::{demux_file_from, PacketQueues};
use glimpse_engine::{Serial, VideoFrameData};

struct PlaybackRun {
    queues: PacketQueues,
    cancelled: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

struct FrameSchedulerState {
    pending: Option<VideoFrameData>,
    present_first_frame: bool,
    displayed_frames: u64,
    dropped_frames: u64,
    tick_count: u64,
    stream_finished: bool,
}

struct SubtitleSchedulerState {
    pending: std::collections::VecDeque<SubtitleCue>,
    active: Option<SubtitleCue>,
}

/// Keep subtitle text proportional to the available video display width.
/// 28 logical pixels is the reference size at a 1280px-wide display; clamp
/// the result so very small windows and ultrawide displays remain readable.
fn apply_subtitle_font_size(label: &gtk4::Label, display_width: i32) {
    let font_size = ((display_width.max(1) as f64 / 1280.0) * 28.0)
        .round()
        .clamp(20.0, 48.0) as i32;
    // Keep attributes created by set_markup() (bold/italic/etc.) and only
    // replace the width-dependent size attribute.
    let attributes = label
        .attributes()
        .unwrap_or_else(gtk4::pango::AttrList::new);
    attributes.change(gtk4::pango::AttrSize::new_size_absolute(
        font_size * gtk4::pango::SCALE,
    ));
    label.set_attributes(Some(&attributes));
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

pub fn bind_subtitle_scaling(picture: &gtk4::Picture, subtitle_label: &gtk4::Label) {
    apply_subtitle_font_size(subtitle_label, picture.width());
    let label = subtitle_label.downgrade();
    picture.connect_notify_local(Some("width"), move |picture, _| {
        if let Some(label) = label.upgrade() {
            apply_subtitle_font_size(&label, picture.width());
        }
    });
}

pub struct VideoPlaybackHandle {
    path: String,
    tracks: TrackList,
    picture: gtk4::Picture,
    subtitle_label: gtk4::Label,
    external_subtitle_path: Option<String>,
    clock: Arc<MasterClock>,
    run: Option<PlaybackRun>,
}

impl VideoPlaybackHandle {
    pub fn seek(&mut self, target: Duration) {
        self.subtitle_label.set_text("");
        self.subtitle_label.set_visible(false);
        self.stop_run();
        if let Err(error) = self.start_run(target) {
            tracing::error!(%error, "Could not restart video after seek");
        }
    }

    pub fn select_subtitle(&mut self, id: Option<glimpse_engine::TrackId>) {
        self.tracks.active_subtitle = id;
        self.external_subtitle_path = None;
        self.subtitle_label.set_text("");
        self.subtitle_label.set_visible(false);
        self.stop_run();
        let position = Duration::from_secs_f64(self.clock.get_pts().max(0.0));
        if let Err(error) = self.start_run(position) {
            tracing::error!(%error, "Could not restart video after subtitle selection");
        }
    }

    pub fn select_external_subtitle(&mut self, path: String) -> Result<(), String> {
        self.tracks.active_subtitle = None;
        self.external_subtitle_path = Some(path);
        self.subtitle_label.set_text("");
        self.subtitle_label.set_visible(false);
        self.stop_run();
        let position = Duration::from_secs_f64(self.clock.get_pts().max(0.0));
        self.start_run(position)
    }

    pub fn stop(&mut self) {
        self.subtitle_label.set_text("");
        self.subtitle_label.set_visible(false);
        self.stop_run();
    }

    fn stop_run(&mut self) {
        if let Some(mut run) = self.run.take() {
            run.cancelled.store(true, Ordering::Release);
            run.queues.abort();
            if let Some(worker) = run.worker.take() {
                let _ = worker.join();
            }
        }
    }

    fn start_run(&mut self, start_position: Duration) -> Result<(), String> {
        let video_stream_index = self.tracks.video_tracks.first()
            .map(|track| track.id.0)
            .ok_or_else(|| "The media has no video stream".to_string())?;
        let queues = PacketQueues::new(64, 64 * 1024 * 1024);
        let cancelled = Arc::new(AtomicBool::new(false));
        let (sender, receiver) = bounded::<Result<VideoFrameData, String>>(2);
        let (subtitle_sender, subtitle_receiver) = unbounded::<Result<SubtitleCue, String>>();
        let worker = spawn_decoder_worker(
            self.path.clone(),
            self.tracks.clone(),
            self.external_subtitle_path.clone(),
            video_stream_index,
            queues.clone(),
            Arc::clone(&cancelled),
            sender,
            subtitle_sender,
            start_position,
        )?;
        schedule_frames(
            receiver,
            subtitle_receiver,
            self.picture.clone(),
            self.subtitle_label.clone(),
            Arc::clone(&self.clock),
            Arc::clone(&cancelled),
        );
        self.run = Some(PlaybackRun { queues, cancelled, worker: Some(worker) });
        Ok(())
    }
}

impl Drop for VideoPlaybackHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

pub fn present_continuous_frames(
    path: &str,
    tracks: TrackList,
    picture: &gtk4::Picture,
    subtitle_label: &gtk4::Label,
    clock: Arc<MasterClock>,
) -> Option<VideoPlaybackHandle> {
    if tracks.video_tracks.is_empty() {
        tracing::warn!("Engine video path skipped: no video stream");
        return None;
    }
    let mut handle = VideoPlaybackHandle {
        path: path.to_owned(), tracks, picture: picture.clone(), subtitle_label: subtitle_label.clone(),
        external_subtitle_path: None, clock, run: None,
    };
    if let Err(error) = handle.start_run(Duration::ZERO) {
        tracing::error!(%error, "Could not start engine video playback");
        return None;
    }
    Some(handle)
}

fn spawn_decoder_worker(
    path: String,
    tracks: TrackList,
    external_subtitle_path: Option<String>,
    video_stream_index: usize,
    queues: PacketQueues,
    cancelled: Arc<AtomicBool>,
    sender: Sender<Result<VideoFrameData, String>>,
    subtitle_sender: Sender<Result<SubtitleCue, String>>,
    start_position: Duration,
) -> Result<JoinHandle<()>, String> {
    thread::Builder::new()
        .name("glimpse-video-decode".to_string())
        .spawn(move || {
            if let Err(error) = decode_continuously(
                &path, &tracks, external_subtitle_path.as_deref(), video_stream_index, &queues, &cancelled, &sender, &subtitle_sender, start_position,
            ) {
                if !cancelled.load(Ordering::Acquire) {
                    let _ = sender.try_send(Err(error));
                }
            }
        })
        .map_err(|error| format!("Could not start video decoder: {error}"))
}

fn decode_continuously(
    path: &str,
    tracks: &TrackList,
    external_subtitle_path: Option<&str>,
    video_stream_index: usize,
    queues: &PacketQueues,
    cancelled: &AtomicBool,
    sender: &Sender<Result<VideoFrameData, String>>,
    subtitle_sender: &Sender<Result<SubtitleCue, String>>,
    start_position: Duration,
) -> Result<(), String> {
    let serial = Serial(0);
    let frame_queue = Arc::new(VideoFrameQueue::new(32, 512 * 1024 * 1024));
    let mut decoder = VideoDecoder::open_ffmpeg(
        path,
        video_stream_index,
        Arc::clone(&queues.video),
        Arc::clone(&frame_queue),
        Arc::new(MasterClock::new()),
    )?;

    let mut subtitle_decoder = if external_subtitle_path.is_none() {
        tracks.active_subtitle
            .map(|track| SubtitleDecoder::open_ffmpeg(path, track.0))
            .transpose()?
    } else {
        None
    };

    if let Some(subtitle_path) = external_subtitle_path {
        match decode_external_subtitle_file(subtitle_path) {
            Ok(cues) => {
                for cue in cues {
                    if cancelled.load(Ordering::Acquire) {
                        return Ok(());
                    }
                    if subtitle_sender.send(Ok(cue)).is_err() {
                        return Ok(());
                    }
                }
            }
            Err(error) => {
                let _ = subtitle_sender.send(Err(error));
            }
        }
    }

    let worker_path = path.to_owned();
    let worker_tracks = tracks.clone();
    let worker_queues = queues.clone();
    let demux_worker = thread::spawn(move || {
        demux_file_from(&worker_path, &worker_tracks, &worker_queues, serial, start_position)
    });

    loop {
        if cancelled.load(Ordering::Acquire) {
            queues.abort();
            frame_queue.abort();
            let _ = demux_worker.join();
            return Ok(());
        }
        if let Err(error) = decoder.decode_step(serial) {
            queues.abort();
            frame_queue.abort();
            let _ = demux_worker.join();
            return Err(error);
        }
        if !forward_decoded_frames(&frame_queue, sender, start_position.as_secs_f64(), cancelled) {
            queues.abort();
            frame_queue.abort();
            let _ = demux_worker.join();
            return Ok(());
        }
        while queues.audio.pop_timeout(Duration::ZERO).is_some() {}
        while let Some(tagged) = queues.subtitle.pop_timeout(Duration::ZERO) {
            if tagged.serial != serial { continue; }
            if let Some(decoder) = subtitle_decoder.as_mut() {
                let cue = match decoder.decode_packet(&tagged.item) {
                    Ok(cue) => cue,
                    Err(error) => {
                        queues.abort();
                        frame_queue.abort();
                        let _ = demux_worker.join();
                        return Err(error);
                    }
                };
                if let Some(cue) = cue {
                    let mut message = Ok(cue);
                    loop {
                        if cancelled.load(Ordering::Acquire) { break; }
                        match subtitle_sender.send_timeout(message, Duration::from_millis(5)) {
                            Ok(()) => break,
                            Err(crossbeam_channel::SendTimeoutError::Timeout(returned)) => message = returned,
                            Err(crossbeam_channel::SendTimeoutError::Disconnected(_)) => break,
                        }
                    }
                }
            }
        }

        if demux_worker.is_finished() && queues.video.stats().0 == 0 {
            let result = demux_worker.join()
                .map_err(|_| "Video demux worker panicked".to_string())?;
            result.map_err(|error| format!("Video demux failed: {error}"))?;
            decoder.finish(serial)?;
            let _ = forward_decoded_frames(&frame_queue, sender, start_position.as_secs_f64(), cancelled);
            return Ok(());
        }
    }
}

fn forward_decoded_frames(
    frame_queue: &VideoFrameQueue,
    sender: &Sender<Result<VideoFrameData, String>>,
    minimum_pts: f64,
    cancelled: &AtomicBool,
) -> bool {
    while let Some(frame) = frame_queue.pop_timeout(Duration::ZERO) {
        if frame.item.pts + 0.000_001 < minimum_pts {
            continue;
        }

        let mut message = Ok(frame.item);
        loop {
            if cancelled.load(Ordering::Acquire) {
                return false;
            }
            match sender.send_timeout(message, Duration::from_millis(5)) {
                Ok(()) => break,
                Err(crossbeam_channel::SendTimeoutError::Timeout(returned)) => {
                    message = returned;
                }
                Err(crossbeam_channel::SendTimeoutError::Disconnected(_)) => return false,
            }
        }
    }
    true
}

fn schedule_frames(
    receiver: Receiver<Result<VideoFrameData, String>>,
    subtitle_receiver: Receiver<Result<SubtitleCue, String>>,
    picture: gtk4::Picture,
    subtitle_label: gtk4::Label,
    clock: Arc<MasterClock>,
    cancelled: Arc<AtomicBool>,
) {
    let state = Rc::new(RefCell::new(FrameSchedulerState {
        pending: None,
        present_first_frame: true,
        displayed_frames: 0,
        dropped_frames: 0,
        tick_count: 0,
        stream_finished: false,
    }));
    let picture_weak = picture.downgrade();
    let subtitle_weak = subtitle_label.downgrade();
    let subtitle_state = RefCell::new(SubtitleSchedulerState {
        pending: std::collections::VecDeque::new(),
        active: None,
    });

    tracing::info!(
        mapped = picture.is_mapped(),
        visible = picture.is_visible(),
        "Starting vblank-aware video presentation scheduler"
    );

    // Keep frame ingestion independent of GDK's visibility-aware frame clock.
    // This timer drains stale frames while the window is on another desktop.
    let ingest_state = Rc::clone(&state);
    let ingest_receiver = receiver.clone();
    let ingest_clock = Arc::clone(&clock);
    let ingest_cancelled = Arc::clone(&cancelled);
    let ingest_picture = picture.downgrade();
    glib::timeout_add_local(Duration::from_millis(5), move || {
        if ingest_cancelled.load(Ordering::Acquire) || ingest_picture.upgrade().is_none() {
            return glib::ControlFlow::Break;
        }

        let mut state = ingest_state.borrow_mut();
        if state.stream_finished {
            return glib::ControlFlow::Break;
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
                    state.dropped_frames += 1;
                    if state.dropped_frames == 1 || state.dropped_frames % 120 == 0 {
                        tracing::warn!(
                            frame_pts,
                            master_pts,
                            lateness_ms = lateness * 1000.0,
                            dropped_frames = state.dropped_frames,
                            queued_frames = ingest_receiver.len(),
                            "Dropping late video frame during independent queue drain"
                        );
                    }
                    continue;
                }
                break;
            }

            match ingest_receiver.try_recv() {
                Ok(Ok(frame)) => state.pending = Some(frame),
                Ok(Err(error)) => {
                    tracing::error!(
                        %error,
                        displayed_frames = state.displayed_frames,
                        dropped_frames = state.dropped_frames,
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

        let (refresh_interval_us, _) = frame_clock.refresh_info(frame_clock.frame_time());
        let refresh_interval_us = if refresh_interval_us > 0 {
            refresh_interval_us
        } else {
            16_667
        };
        let presentation_window = refresh_interval_us as f64 / 1_000_000.0;
        let master_pts = clock.get_pts();

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
        if let Some(cue) = subtitle_state.active.as_ref() {
            subtitle_label.set_markup(&subtitle_cue_markup(cue));
            apply_subtitle_font_size(&subtitle_label, picture.width());
            subtitle_label.set_visible(true);
        } else {
            subtitle_label.set_visible(false);
        }

        let mut state = state.borrow_mut();
        state.tick_count += 1;
        if state.tick_count % 250 == 0 {
            tracing::info!(
                ticks = state.tick_count,
                displayed_frames = state.displayed_frames,
                dropped_frames = state.dropped_frames,
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
            let dropped_before = state.dropped_frames;
            let mut candidate = state.pending.take();

            loop {
                let frame = if let Some(frame) = candidate.take() {
                    frame
                } else {
                    match receiver.try_recv() {
                        Ok(Ok(frame)) => frame,
                        Ok(Err(error)) => {
                            tracing::error!(
                                %error,
                                displayed_frames = state.displayed_frames,
                                dropped_frames = state.dropped_frames,
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
                    state.dropped_frames += 1;
                    continue;
                }

                if frame.pts <= master_pts + presentation_window {
                    if selected_frame.replace(frame).is_some() {
                        state.dropped_frames += 1;
                    }
                } else {
                    // Frames arrive in presentation order. Keep the first
                    // future frame and leave the rest queued for later ticks.
                    state.pending = Some(frame);
                    break;
                }
            }

            if state.dropped_frames / 120 != dropped_before / 120 {
                tracing::debug!(
                    dropped_frames = state.dropped_frames,
                    displayed_frames = state.displayed_frames,
                    queued_frames = receiver.len(),
                    master_pts,
                    "Skipped superseded or late video frames during presentation"
                );
            }
        }

        let Some(frame) = selected_frame else {
            if state.stream_finished && state.pending.is_none() {
                subtitle_label.set_text("");
                subtitle_label.set_visible(false);
                tracing::info!(
                    displayed_frames = state.displayed_frames,
                    dropped_frames = state.dropped_frames,
                    ticks = state.tick_count,
                    "Engine video stream finished; stopping frame scheduler"
                );
                return glib::ControlFlow::Break;
            }
            return glib::ControlFlow::Continue;
        };

        let texture = match frame.vaapi_frame.as_ref() {
                Some(vaapi_frame) => {
                    #[cfg(feature = "vaapi-hdr")]
                    {
                        match crate::vaapi_texture::create_texture(vaapi_frame) {
                            Ok(texture) => texture,
                            Err(error) => {
                                tracing::error!(%error, pts = frame.pts, "Could not import VAAPI frame into GTK");
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
                    .upcast()
                }
            };
        picture.set_paintable(Some(&texture));
        state.displayed_frames += 1;
        if state.displayed_frames == 1 {
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
