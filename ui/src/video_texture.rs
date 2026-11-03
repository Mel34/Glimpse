//! Experimental continuous video presentation through GTK4 textures.
//!
//! FFmpeg demuxing and software decoding run on worker threads. GTK owns the
//! presentation timer and creates GdkMemoryTexture objects on its main thread.
//! GtkMediaFile remains the audio clock and fallback playback implementation.

use glimpse_engine::TrackList;

#[cfg(feature = "experimental-engine")]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(feature = "experimental-engine")]
use std::sync::Arc;
#[cfg(feature = "experimental-engine")]
use std::thread;
#[cfg(feature = "experimental-engine")]
use std::time::Duration;

#[cfg(feature = "experimental-engine")]
use crossbeam_channel::{bounded, Receiver, Sender, TryRecvError};
#[cfg(feature = "experimental-engine")]
use glimpse_engine::decoder::{VideoDecoder, VideoFrameQueue};
#[cfg(feature = "experimental-engine")]
use glimpse_engine::demuxer::{demux_file, PacketQueues};
#[cfg(feature = "experimental-engine")]
use glimpse_engine::{MasterClock, Serial, VideoFrameData};

pub struct VideoPlaybackHandle {
    #[cfg(feature = "experimental-engine")]
    queues: PacketQueues,
    #[cfg(feature = "experimental-engine")]
    cancelled: Arc<AtomicBool>,
}

impl VideoPlaybackHandle {
    pub fn stop(&self) {
        #[cfg(feature = "experimental-engine")]
        {
            self.cancelled.store(true, Ordering::Release);
            self.queues.abort();
        }
    }
}

impl Drop for VideoPlaybackHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Starts continuous software decoding and schedules frames against the
/// GtkMediaFile timestamp. Returns None in the normal, non-experimental build.
pub fn present_continuous_frames(
    path: &str,
    tracks: TrackList,
    picture: &gtk4::Picture,
    media: &gtk4::MediaFile,
) -> Option<VideoPlaybackHandle> {
    #[cfg(feature = "experimental-engine")]
    {
        let video_track = match tracks.video_tracks.first() {
            Some(track) => track,
            None => {
                tracing::warn!("Experimental video path skipped: no video stream");
                return None;
            }
        };

        let queues = PacketQueues::new(64, 64 * 1024 * 1024);
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_cancelled = Arc::clone(&cancelled);
        let (sender, receiver) = bounded::<Result<VideoFrameData, String>>(2);
        spawn_decoder_worker(
            path.to_owned(),
            tracks,
            video_track.id.0,
            queues.clone(),
            worker_cancelled,
            sender,
        );

        schedule_frames(receiver, picture.clone(), media.clone(), Arc::clone(&cancelled));

        Some(VideoPlaybackHandle { queues, cancelled })
    }

    #[cfg(not(feature = "experimental-engine"))]
    {
        let _ = (path, tracks, picture, media);
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
) {
    thread::spawn(move || {
        if let Err(error) = decode_continuously(
            &path,
            &tracks,
            video_stream_index,
            &queues,
            &cancelled,
            &sender,
        ) {
            if !cancelled.load(Ordering::Acquire) {
                let _ = sender.send(Err(error));
            }
        }
    });
}

#[cfg(feature = "experimental-engine")]
fn decode_continuously(
    path: &str,
    tracks: &TrackList,
    video_stream_index: usize,
    queues: &PacketQueues,
    cancelled: &AtomicBool,
    sender: &Sender<Result<VideoFrameData, String>>,
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
        demux_file(&worker_path, &worker_tracks, &worker_queues, serial)
    });

    loop {
        if cancelled.load(Ordering::Acquire) {
            queues.abort();
            let _ = demux_worker.join();
            return Ok(());
        }

        if let Err(error) = decoder.decode_step(serial) {
            queues.abort();
            let _ = demux_worker.join();
            return Err(error);
        }

        if !forward_decoded_frames(&frame_queue, sender)? {
            queues.abort();
            let _ = demux_worker.join();
            return Ok(());
        }

        // This bridge presents video only. Drain the other stream queues so
        // the demux worker cannot stall behind audio/subtitle backpressure.
        while queues.audio.pop_timeout(Duration::ZERO).is_some() {}
        while queues.subtitle.pop_timeout(Duration::ZERO).is_some() {}

        if demux_worker.is_finished() && queues.video.stats().0 == 0 {
            let demux_result = demux_worker
                .join()
                .map_err(|_| "Demux worker panicked".to_string())?;
            demux_result.map_err(|error| format!("Media demux failed: {error}"))?;

            decoder.finish(serial)?;
            if !forward_decoded_frames(&frame_queue, sender)? {
                return Ok(());
            }
            return Ok(());
        }
    }
}

#[cfg(feature = "experimental-engine")]
fn forward_decoded_frames(
    frame_queue: &VideoFrameQueue,
    sender: &Sender<Result<VideoFrameData, String>>,
) -> Result<bool, String> {
    while let Some(frame) = frame_queue.pop_timeout(Duration::ZERO) {
        if sender.send(Ok(frame.item)).is_err() {
            return Ok(false);
        }
    }
    Ok(true)
}

#[cfg(feature = "experimental-engine")]
fn schedule_frames(
    receiver: Receiver<Result<VideoFrameData, String>>,
    picture: gtk4::Picture,
    fallback_media: gtk4::MediaFile,
    cancelled: Arc<AtomicBool>,
) {
    let mut pending: Option<VideoFrameData> = None;
    let mut displayed_frames = 0u64;
    let mut dropped_frames = 0u64;

    glib::timeout_add_local(Duration::from_millis(5), move || {
        if cancelled.load(Ordering::Acquire) {
            return glib::ControlFlow::Break;
        }

        loop {
            if pending.is_none() {
                match receiver.try_recv() {
                    Ok(Ok(frame)) => pending = Some(frame),
                    Ok(Err(error)) => {
                        tracing::error!(%error, "Experimental video decoder failed; restoring GTK playback");
                        picture.set_paintable(Some(&fallback_media));
                        return glib::ControlFlow::Break;
                    }
                    Err(TryRecvError::Empty) => return glib::ControlFlow::Continue,
                    Err(TryRecvError::Disconnected) => {
                        tracing::info!(
                            displayed_frames,
                            dropped_frames,
                            "Experimental video stream finished"
                        );
                        return glib::ControlFlow::Break;
                    }
                }
            }

            let Some(frame) = pending.as_ref() else {
                return glib::ControlFlow::Continue;
            };

            // GtkMediaFile is still playing the audio and provides the shared
            // media timeline. PTS values from the decoder use seconds.
            let audio_pts = fallback_media.timestamp().max(0) as f64 / 1_000_000.0;
            let lateness = audio_pts - frame.pts;
            let late_limit = frame.duration.max(0.040) * 2.0;

            if lateness > late_limit {
                pending = None;
                dropped_frames += 1;
                continue;
            }

            if frame.pts <= audio_pts + 0.005 {
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
