//! Engine-clock-driven video presentation through GTK4 textures.

use glimpse_engine::{MasterClock, TrackList};
use glib::object::ObjectExt;
use gtk4::prelude::WidgetExtManual;
use std::cell::RefCell;
use std::sync::Arc;
use std::time::Duration;

use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use crossbeam_channel::{bounded, unbounded, Receiver, Sender, TryRecvError};
use glimpse_engine::decoder::{SubtitleCue, SubtitleDecoder, VideoDecoder, VideoFrameQueue};
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
}

struct SubtitleSchedulerState {
    pending: std::collections::VecDeque<SubtitleCue>,
    active: Option<SubtitleCue>,
}

pub struct VideoPlaybackHandle {
    path: String,
    tracks: TrackList,
    picture: gtk4::Picture,
    subtitle_label: gtk4::Label,
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
        self.subtitle_label.set_text("");
        self.subtitle_label.set_visible(false);
        self.stop_run();
        let position = Duration::from_secs_f64(self.clock.get_pts().max(0.0));
        if let Err(error) = self.start_run(position) {
            tracing::error!(%error, "Could not restart video after subtitle selection");
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
        path: path.to_owned(), tracks, picture: picture.clone(), subtitle_label: subtitle_label.clone(), clock,
        run: None,
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
                &path, &tracks, video_stream_index, &queues, &cancelled, &sender, &subtitle_sender, start_position,
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

    let mut subtitle_decoder = tracks.active_subtitle
        .map(|track| SubtitleDecoder::open_ffmpeg(path, track.0))
        .transpose()?;

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
    let state = RefCell::new(FrameSchedulerState {
        pending: None,
        present_first_frame: true,
        displayed_frames: 0,
        dropped_frames: 0,
    });
    // A weak reference avoids a Picture -> tick callback -> Picture cycle.
    let picture_weak = picture.downgrade();
    let subtitle_weak = subtitle_label.downgrade();
    let subtitle_state = RefCell::new(SubtitleSchedulerState {
        pending: std::collections::VecDeque::new(),
        active: None,
    });

    // GTK tick callbacks are Fn, not FnMut, so keep per-run scheduler state
    // behind RefCell and update the Picture through the callback's weak ref.
    picture.add_tick_callback(move |_, _frame_clock| {
        if cancelled.load(Ordering::Acquire) {
            return glib::ControlFlow::Break;
        }
        let Some(picture) = picture_weak.upgrade() else {
            return glib::ControlFlow::Break;
        };
        let Some(subtitle_label) = subtitle_weak.upgrade() else {
            return glib::ControlFlow::Break;
        };
        let master_pts = clock.get_pts();
        let mut subtitle_state = subtitle_state.borrow_mut();
        loop {
            match subtitle_receiver.try_recv() {
                Ok(Ok(cue)) => subtitle_state.pending.push_back(cue),
                Ok(Err(error)) => tracing::warn!(%error, "Embedded subtitle decoder failed"),
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
            subtitle_label.set_text(&cue.text);
            subtitle_label.set_visible(true);
        } else {
            subtitle_label.set_visible(false);
        }
        let mut state = state.borrow_mut();
        loop {
            if state.pending.is_none() {
                match receiver.try_recv() {
                    Ok(Ok(frame)) => state.pending = Some(frame),
                    Ok(Err(error)) => {
                        tracing::error!(%error, "Engine video decoder failed");
                        return glib::ControlFlow::Break;
                    }
                    Err(TryRecvError::Empty) => return glib::ControlFlow::Continue,
                    Err(TryRecvError::Disconnected) => {
                        subtitle_label.set_text("");
                        subtitle_label.set_visible(false);
                        tracing::info!(
                            displayed_frames = state.displayed_frames,
                            dropped_frames = state.dropped_frames,
                            "Engine video stream finished"
                        );
                        return glib::ControlFlow::Break;
                    }
                }
            }
            let Some(frame) = state.pending.as_ref() else {
                return glib::ControlFlow::Continue;
            };
            let master_pts = clock.get_pts();
            let lateness = master_pts - frame.pts;
            if !state.present_first_frame && lateness > 0.040 {
                state.pending = None;
                state.dropped_frames += 1;
                continue;
            }
            if state.present_first_frame || frame.pts <= master_pts + 0.005 {
                let frame = state.pending.take().expect("pending frame was checked");
                state.present_first_frame = false;
                // The decoded frame normally has a single owner here. Transfer
                // its pixel allocation into GBytes instead of cloning every frame.
                let pixels = Arc::try_unwrap(frame.rgba_data)
                    .unwrap_or_else(|shared| shared.as_ref().clone());
                let bytes = glib::Bytes::from_owned(pixels);
                let texture = gdk4::MemoryTexture::new(
                    frame.width as i32,
                    frame.height as i32,
                    gdk4::MemoryFormat::R8g8b8a8,
                    &bytes,
                    frame.stride,
                );
                picture.set_paintable(Some(&texture));
                state.displayed_frames += 1;
            }
            return glib::ControlFlow::Continue;
        }
    });
}
