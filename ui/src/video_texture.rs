//! Engine-clock-driven video presentation through GTK4 textures.

use glimpse_engine::{MasterClock, TrackList};
use std::sync::Arc;
use std::time::Duration;
use gtk4::prelude::RangeExt;

#[cfg(feature = "experimental-engine")]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(feature = "experimental-engine")]
use std::thread::{self, JoinHandle};
#[cfg(feature = "experimental-engine")]
use crossbeam_channel::{bounded, Receiver, Sender, TryRecvError};
#[cfg(feature = "experimental-engine")]
use glimpse_engine::decoder::{VideoDecoder, VideoFrameQueue};
#[cfg(feature = "experimental-engine")]
use glimpse_engine::demuxer::{demux_file_from, PacketQueues};
#[cfg(feature = "experimental-engine")]
use glimpse_engine::{Serial, VideoFrameData};

#[cfg(feature = "experimental-engine")]
struct PlaybackRun {
    queues: PacketQueues,
    cancelled: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

pub struct VideoPlaybackHandle {
    #[cfg(feature = "experimental-engine")]
    path: String,
    #[cfg(feature = "experimental-engine")]
    tracks: TrackList,
    #[cfg(feature = "experimental-engine")]
    picture: gtk4::Picture,
    #[cfg(feature = "experimental-engine")]
    clock: Arc<MasterClock>,
    #[cfg(feature = "experimental-engine")]
    seekbar: gtk4::Scale,
    #[cfg(feature = "experimental-engine")]
    elapsed_label: gtk4::Label,
    #[cfg(feature = "experimental-engine")]
    run: Option<PlaybackRun>,
}

impl VideoPlaybackHandle {
    pub fn seek(&mut self, target: Duration) {
        #[cfg(feature = "experimental-engine")]
        {
            self.stop_run();
            if let Err(error) = self.start_run(target) {
                tracing::error!(%error, "Could not restart video after seek");
            }
        }
        #[cfg(not(feature = "experimental-engine"))]
        let _ = target;
    }

    pub fn stop(&mut self) {
        #[cfg(feature = "experimental-engine")]
        self.stop_run();
    }

    #[cfg(feature = "experimental-engine")]
    fn stop_run(&mut self) {
        if let Some(mut run) = self.run.take() {
            run.cancelled.store(true, Ordering::Release);
            run.queues.abort();
            if let Some(worker) = run.worker.take() {
                let _ = worker.join();
            }
        }
    }

    #[cfg(feature = "experimental-engine")]
    fn start_run(&mut self, start_position: Duration) -> Result<(), String> {
        let video_stream_index = self.tracks.video_tracks.first()
            .map(|track| track.id.0)
            .ok_or_else(|| "The media has no video stream".to_string())?;
        let queues = PacketQueues::new(64, 64 * 1024 * 1024);
        let cancelled = Arc::new(AtomicBool::new(false));
        let (sender, receiver) = bounded::<Result<VideoFrameData, String>>(2);
        let worker = spawn_decoder_worker(
            self.path.clone(),
            self.tracks.clone(),
            video_stream_index,
            queues.clone(),
            Arc::clone(&cancelled),
            sender,
            start_position,
        )?;
        schedule_frames(
            receiver,
            self.picture.clone(),
            Arc::clone(&self.clock),
            Arc::clone(&cancelled),
            self.seekbar.clone(),
            self.elapsed_label.clone(),
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
    clock: Arc<MasterClock>,
    seekbar: &gtk4::Scale,
    elapsed_label: &gtk4::Label,
) -> Option<VideoPlaybackHandle> {
    #[cfg(feature = "experimental-engine")]
    {
        if tracks.video_tracks.is_empty() {
            tracing::warn!("Engine video path skipped: no video stream");
            return None;
        }
        let mut handle = VideoPlaybackHandle {
            path: path.to_owned(),
            tracks,
            picture: picture.clone(),
            clock,
            seekbar: seekbar.clone(),
            elapsed_label: elapsed_label.clone(),
            run: None,
        };
        if let Err(error) = handle.start_run(Duration::ZERO) {
            tracing::error!(%error, "Could not start engine video playback");
            return None;
        }
        Some(handle)
    }
    #[cfg(not(feature = "experimental-engine"))]
    {
        let _ = (path, tracks, picture, clock, seekbar, elapsed_label);
        None
    }
}

#[cfg(feature = "experimental-engine")]
fn spawn_decoder_worker(
    path: String,
    tracks: TrackList,
    video_stream_index: usize,
    queues: PacketQueues,
    cancelled: Arc<AtomicBool>,
    sender: Sender<Result<VideoFrameData, String>>,
    start_position: Duration,
) -> Result<JoinHandle<()>, String> {
    thread::Builder::new()
        .name("glimpse-video-decode".to_string())
        .spawn(move || {
            if let Err(error) = decode_continuously(
                &path, &tracks, video_stream_index, &queues, &cancelled, &sender, start_position,
            ) {
                if !cancelled.load(Ordering::Acquire) {
                    let _ = sender.try_send(Err(error));
                }
            }
        })
        .map_err(|error| format!("Could not start video decoder: {error}"))
}

#[cfg(feature = "experimental-engine")]
fn decode_continuously(
    path: &str,
    tracks: &TrackList,
    video_stream_index: usize,
    queues: &PacketQueues,
    cancelled: &AtomicBool,
    sender: &Sender<Result<VideoFrameData, String>>,
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
        if !forward_decoded_frames(&frame_queue, sender, start_position.as_secs_f64()) {
            queues.abort();
            frame_queue.abort();
            let _ = demux_worker.join();
            return Ok(());
        }
        while queues.audio.pop_timeout(Duration::ZERO).is_some() {}
        while queues.subtitle.pop_timeout(Duration::ZERO).is_some() {}

        if demux_worker.is_finished() && queues.video.stats().0 == 0 {
            let result = demux_worker.join()
                .map_err(|_| "Video demux worker panicked".to_string())?;
            result.map_err(|error| format!("Video demux failed: {error}"))?;
            decoder.finish(serial)?;
            let _ = forward_decoded_frames(&frame_queue, sender, start_position.as_secs_f64());
            return Ok(());
        }
    }
}

#[cfg(feature = "experimental-engine")]
fn forward_decoded_frames(
    frame_queue: &VideoFrameQueue,
    sender: &Sender<Result<VideoFrameData, String>>,
    minimum_pts: f64,
) -> bool {
    while let Some(frame) = frame_queue.pop_timeout(Duration::ZERO) {
        if frame.item.pts + 0.000_001 < minimum_pts {
            continue;
        }
        match sender.try_send(Ok(frame.item)) {
            Ok(()) | Err(crossbeam_channel::TrySendError::Full(_)) => {}
            Err(crossbeam_channel::TrySendError::Disconnected(_)) => return false,
        }
    }
    true
}

#[cfg(feature = "experimental-engine")]
fn schedule_frames(
    receiver: Receiver<Result<VideoFrameData, String>>,
    picture: gtk4::Picture,
    clock: Arc<MasterClock>,
    cancelled: Arc<AtomicBool>,
    seekbar: gtk4::Scale,
    elapsed_label: gtk4::Label,
) {
    let mut pending: Option<VideoFrameData> = None;
    let mut displayed_frames = 0u64;
    let mut dropped_frames = 0u64;

    glib::timeout_add_local(Duration::from_millis(5), move || {
        if cancelled.load(Ordering::Acquire) {
            return glib::ControlFlow::Break;
        }
        let position = clock.get_pts().max(0.0);
        seekbar.set_value(position);
        elapsed_label.set_text(&crate::controls::format_duration(Duration::from_secs_f64(position)));

        loop {
            if pending.is_none() {
                match receiver.try_recv() {
                    Ok(Ok(frame)) => pending = Some(frame),
                    Ok(Err(error)) => {
                        tracing::error!(%error, "Engine video decoder failed");
                        return glib::ControlFlow::Break;
                    }
                    Err(TryRecvError::Empty) => return glib::ControlFlow::Continue,
                    Err(TryRecvError::Disconnected) => {
                        tracing::info!(displayed_frames, dropped_frames, "Engine video stream finished");
                        return glib::ControlFlow::Break;
                    }
                }
            }
            let Some(frame) = pending.as_ref() else {
                return glib::ControlFlow::Continue;
            };
            let master_pts = clock.get_pts();
            let lateness = master_pts - frame.pts;
            if lateness > 0.040 {
                pending = None;
                dropped_frames += 1;
                continue;
            }
            if frame.pts <= master_pts + 0.005 {
                let frame = pending.take().expect("pending frame was checked");
                let bytes = glib::Bytes::from_owned(frame.rgba_data.as_ref().clone());
                let texture = gdk4::MemoryTexture::new(
                    frame.width as i32,
                    frame.height as i32,
                    gdk4::MemoryFormat::R8g8b8a8,
                    &bytes,
                    frame.stride,
                );
                picture.set_paintable(Some(&texture));
                displayed_frames += 1;
            }
            return glib::ControlFlow::Continue;
        }
    });
}
