//! Experimental bridge from the software decoder to a GTK4 Picture.
//!
//! This deliberately presents only the first decoded frame. It proves the
//! FFmpeg -> RGBA -> GdkMemoryTexture -> GTK path without changing the
//! established GtkMediaFile playback path.

use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crossbeam_channel::{bounded, TryRecvError};
use glimpse_engine::decoder::{VideoDecoder, VideoFrameQueue};
use glimpse_engine::demuxer::{demux_file, PacketQueues};
use glimpse_engine::{MasterClock, TrackList, VideoFrameData, Serial};
use gtk4::Picture;

pub fn present_first_decoded_frame(path: &str, tracks: TrackList, picture: &Picture) {
    let (sender, receiver) = bounded::<Result<VideoFrameData, String>>(1);
    let path = path.to_owned();

    thread::spawn(move || {
        if let Err(error) = decode_first_frame(&path, &tracks, &sender) {
            let _ = sender.try_send(Err(error));
        }
    });

    let picture = picture.clone();
    glib::timeout_add_local(Duration::from_millis(16), move || {
        match receiver.try_recv() {
            Ok(Ok(frame)) => {
                let bytes = glib::Bytes::from_owned(frame.rgba_data.as_ref().clone());
                let texture = gdk4::MemoryTexture::new(
                    frame.width as i32,
                    frame.height as i32,
                    gdk4::MemoryFormat::R8g8b8a8,
                    &bytes,
                    frame.stride,
                );
                picture.set_paintable(Some(&texture));
                tracing::info!(
                    width = frame.width,
                    height = frame.height,
                    pts = frame.pts,
                    "Presented first software-decoded frame through GdkMemoryTexture"
                );
                glib::ControlFlow::Break
            }
            Ok(Err(error)) => {
                tracing::error!(%error, "Experimental video presentation failed");
                glib::ControlFlow::Break
            }
            Err(TryRecvError::Empty) => glib::ControlFlow::Continue,
            Err(TryRecvError::Disconnected) => glib::ControlFlow::Break,
        }
    });
}

fn decode_first_frame(
    path: &str,
    tracks: &TrackList,
    sender: &crossbeam_channel::Sender<Result<VideoFrameData, String>>,
) -> Result<(), String> {
    let video_track = tracks
        .video_tracks
        .first()
        .ok_or_else(|| "The file has no video stream to present".to_string())?;

    let serial = Serial(0);
    let queues = PacketQueues::new(64, 64 * 1024 * 1024);
    let frame_queue = Arc::new(VideoFrameQueue::new(32, 512 * 1024 * 1024));
    let mut decoder = VideoDecoder::open_ffmpeg(
        path,
        video_track.id.0,
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
        if let Err(error) = decoder.decode_step(serial) {
            queues.abort();
            let _ = demux_worker.join();
            return Err(error);
        }

        if let Some(frame) = frame_queue.pop_timeout(Duration::ZERO) {
            let _ = sender.try_send(Ok(frame.item));
            // This is a one-frame presentation proof. Stop the demux worker
            // promptly instead of decoding the entire file in the background.
            queues.abort();
            let _ = demux_worker.join();
            return Ok(());
        }

        // The demuxer routes every supported stream. Drain streams that this
        // video-only bridge does not yet decode so they cannot apply backpressure.
        while queues.audio.pop_timeout(Duration::ZERO).is_some() {}
        while queues.subtitle.pop_timeout(Duration::ZERO).is_some() {}

        if demux_worker.is_finished() && queues.video.stats().0 == 0 {
            let demux_result = demux_worker
                .join()
                .map_err(|_| "Demux worker panicked".to_string())?;
            demux_result.map_err(|error| format!("Media demux failed: {error}"))?;

            decoder.finish(serial)?;
            if let Some(frame) = frame_queue.pop_timeout(Duration::ZERO) {
                let _ = sender.try_send(Ok(frame.item));
                return Ok(());
            }
            return Err("The video stream produced no decoded frames".to_string());
        }
    }
}
