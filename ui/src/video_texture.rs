//! Engine-clock-driven video presentation through GTK4 textures.

use crate::presentation_sink::GtkPresentationSink;
use glimpse_engine::{MasterClock, TrackList};
use glib::object::ObjectExt;
use gtk4::prelude::WidgetExt;
use std::sync::Arc;
use std::time::Duration;

use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use crossbeam_channel::{bounded, unbounded, Sender};
use glimpse_engine::decoder::{decode_external_subtitle_file, SubtitleCue, SubtitleDecoder, VideoDecoder, VideoFrameQueue};
use glimpse_engine::demuxer::{demux_file_from, PacketQueues};
use glimpse_engine::{Serial, VideoFrameData};

struct PlaybackRun {
    queues: PacketQueues,
    cancelled: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

/// Keep subtitle text proportional to the available video display width.
/// 28 logical pixels is the reference size at a 1280px-wide display; clamp
/// the result so very small windows and ultrawide displays remain readable.
pub(super) fn apply_subtitle_font_size(label: &gtk4::Label, display_width: i32) {
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
    window_active: bool,
    run: Option<PlaybackRun>,
}

impl VideoPlaybackHandle {
    pub fn seek(&mut self, target: Duration) {
        self.subtitle_label.set_text("");
        self.subtitle_label.set_visible(false);
        self.stop_run();
        if self.window_active {
            if let Err(error) = self.start_run(target, None) {
                tracing::error!(%error, "Could not restart video after seek");
            }
        }
    }

    /// Restart video at a seek target and report when its first frame is
    /// presented by GTK. Returns None when video presentation is inactive.
    pub fn seek_for_sync(&mut self, target: Duration) -> Option<crossbeam_channel::Receiver<()>> {
        self.subtitle_label.set_text("");
        self.subtitle_label.set_visible(false);
        self.stop_run();
        if !self.window_active {
            return None;
        }
        let (ready_sender, ready_receiver) = bounded(1);
        if let Err(error) = self.start_run(target, Some(ready_sender)) {
            tracing::error!(%error, "Could not restart video for synchronized loop");
            return None;
        }
        Some(ready_receiver)
    }

    pub fn select_subtitle(&mut self, id: Option<glimpse_engine::TrackId>) {
        self.tracks.active_subtitle = id;
        self.external_subtitle_path = None;
        self.subtitle_label.set_text("");
        self.subtitle_label.set_visible(false);
        self.stop_run();
        if self.window_active {
            let position = Duration::from_secs_f64(self.clock.get_pts().max(0.0));
            if let Err(error) = self.start_run(position, None) {
                tracing::error!(%error, "Could not restart video after subtitle selection");
            }
        }
    }

    pub fn select_external_subtitle(&mut self, path: String) -> Result<(), String> {
        self.tracks.active_subtitle = None;
        self.external_subtitle_path = Some(path);
        self.subtitle_label.set_text("");
        self.subtitle_label.set_visible(false);
        self.stop_run();
        if self.window_active {
            let position = Duration::from_secs_f64(self.clock.get_pts().max(0.0));
            self.start_run(position, None)
        } else {
            Ok(())
        }
    }

    /// Stop producing video frames while the window is hidden on another
    /// workspace. Restart from the live media clock when it becomes active.
    pub fn set_window_active(&mut self, active: bool) {
        if self.window_active == active {
            return;
        }
        self.window_active = active;
        if !active {
            self.stop_run();
        } else if self.run.is_none() {
            let position = Duration::from_secs_f64(self.clock.get_pts().max(0.0));
            if let Err(error) = self.start_run(position, None) {
                tracing::error!(%error, "Could not resume video after window activation");
            }
        }
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

    fn start_run(&mut self, start_position: Duration, first_frame_ready: Option<Sender<()>>) -> Result<(), String> {
        if !self.window_active {
            return Ok(());
        }
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
        GtkPresentationSink::start(
            receiver,
            subtitle_receiver,
            self.picture.clone(),
            self.subtitle_label.clone(),
            Arc::clone(&self.clock),
            Arc::clone(&cancelled),
            first_frame_ready,
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
    window_active: bool,
) -> Option<VideoPlaybackHandle> {
    if tracks.video_tracks.is_empty() {
        tracing::warn!("Engine video path skipped: no video stream");
        return None;
    }
    let mut handle = VideoPlaybackHandle {
        path: path.to_owned(), tracks, picture: picture.clone(), subtitle_label: subtitle_label.clone(),
        external_subtitle_path: None, clock, window_active, run: None,
    };
    if let Err(error) = handle.start_run(Duration::ZERO, None) {
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

