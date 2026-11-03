//! Glimpse CLI container-probe and software-decoding diagnostic utility.

use std::env;
use std::process;
#[cfg(feature = "ffmpeg-system")]
use std::sync::Arc;
#[cfg(feature = "ffmpeg-system")]
use std::thread;
#[cfg(feature = "ffmpeg-system")]
use std::time::Duration;

use glimpse_engine::{demuxer, Player, TrackInfo};

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let play_audio = args.iter().any(|arg| arg == "--play");
    let decode = args.iter().any(|arg| arg == "--decode");
    let decode_audio = args.iter().any(|arg| arg == "--decode-audio");
    let file_path = args.iter().find(|arg| !arg.starts_with("--"));

    let Some(file_path) = file_path else {
        eprintln!("Usage: glimpse_probe [--play] [--decode] [--decode-audio] <file>");
        process::exit(2);
    };

    let probe = match demuxer::probe_media(file_path) {
        Ok(probe) => probe,
        Err(error) => {
            eprintln!("Probe failed: {error}");
            process::exit(1);
        }
    };

    match probe.duration {
        Some(duration) => println!("Duration: {:.3}s", duration.as_secs_f64()),
        None => println!("Duration: unknown"),
    }
    println!(
        "Streams: {} video, {} audio, {} subtitle",
        probe.tracks.video_tracks.len(),
        probe.tracks.audio_tracks.len(),
        probe.tracks.subtitle_tracks.len()
    );

    print_tracks("Video", &probe.tracks.video_tracks);
    print_tracks("Audio", &probe.tracks.audio_tracks);
    print_tracks("Subtitle", &probe.tracks.subtitle_tracks);

    if decode {
        #[cfg(feature = "ffmpeg-system")]
        if let Err(error) = run_decode_smoke(file_path, &probe.tracks) {
            eprintln!("Decode smoke test failed: {error}");
            process::exit(1);
        }

        #[cfg(not(feature = "ffmpeg-system"))]
        {
            eprintln!("--decode requires --features glimpse-engine/ffmpeg-system");
            process::exit(2);
        }
    }

    if decode_audio {
        #[cfg(feature = "ffmpeg-system")]
        if let Err(error) = run_audio_decode_smoke(file_path, &probe.tracks) {
            eprintln!("Audio decode smoke test failed: {error}");
            process::exit(1);
        }

        #[cfg(not(feature = "ffmpeg-system"))]
        {
            eprintln!("--decode-audio requires --features glimpse-engine/ffmpeg-system");
            process::exit(2);
        }
    }

    if play_audio {
        match Player::open(file_path) {
            Ok((player, _)) => {
                player.play();
                println!(
                    "Engine state demonstration only (no decoding/output yet): {:?}",
                    player.state()
                );
            }
            Err(error) => {
                eprintln!("Could not initialise player: {error}");
                process::exit(1);
            }
        }
    }
}

#[cfg(feature = "ffmpeg-system")]
fn run_decode_smoke(
    file_path: &str,
    tracks: &glimpse_engine::TrackList,
) -> Result<(), String> {
    use glimpse_engine::decoder::{VideoDecoder, VideoFrameQueue};
    use glimpse_engine::demuxer::{demux_file, PacketQueues};
    use glimpse_engine::{MasterClock, Serial};

    let video_track = tracks
        .video_tracks
        .first()
        .ok_or_else(|| "The file has no video stream to decode".to_string())?;
    let serial = Serial(0);
    let queues = PacketQueues::new(64, 64 * 1024 * 1024);
    let frame_queue = Arc::new(VideoFrameQueue::new(32, 512 * 1024 * 1024));
    let mut decoder = VideoDecoder::open_ffmpeg(
        file_path,
        video_track.id.0,
        Arc::clone(&queues.video),
        Arc::clone(&frame_queue),
        Arc::new(MasterClock::new()),
    )?;

    let path = file_path.to_owned();
    let worker_tracks = tracks.clone();
    let worker_queues = queues.clone();
    let worker = thread::spawn(move || {
        demux_file(&path, &worker_tracks, &worker_queues, serial)
    });

    let mut decoded_frames = 0u64;
    let mut first_frame = None;

    // Drain all stream queues while demuxing. This keeps unrelated audio and
    // subtitle streams from applying backpressure to this video-only smoke run.
    loop {
        if let Err(error) = decoder.decode_step(serial) {
            queues.abort();
            let _ = worker.join();
            return Err(error);
        }

        while let Some(frame) = frame_queue.pop_timeout(Duration::from_millis(0)) {
            if first_frame.is_none() {
                first_frame = Some((frame.item.width, frame.item.height, frame.item.pts));
            }
            decoded_frames += 1;
        }
        while queues.audio.pop_timeout(Duration::from_millis(0)).is_some() {}
        while queues.subtitle.pop_timeout(Duration::from_millis(0)).is_some() {}

        if worker.is_finished() && queues.video.stats().0 == 0 {
            break;
        }
    }

    let demux_stats = worker
        .join()
        .map_err(|_| "Demux worker panicked".to_string())?
        .map_err(|error| format!("Demux failed: {error}"))?;

    decoder.finish(serial)?;
    while let Some(frame) = frame_queue.pop_timeout(Duration::from_millis(0)) {
        if first_frame.is_none() {
            first_frame = Some((frame.item.width, frame.item.height, frame.item.pts));
        }
        decoded_frames += 1;
    }

    println!(
        "Demuxed {} video packets ({} bytes across all streams); decoded {} video frames",
        demux_stats.video_packets, demux_stats.bytes_read, decoded_frames
    );
    if let Some((width, height, pts)) = first_frame {
        println!("First decoded frame: {width}x{height} at {pts:.3}s (RGBA)");
    }
    if decoded_frames == 0 {
        return Err("The video stream produced no decoded frames".to_string());
    }
    Ok(())
}

#[cfg(feature = "ffmpeg-system")]
fn run_audio_decode_smoke(
    file_path: &str,
    tracks: &glimpse_engine::TrackList,
) -> Result<(), String> {
    use glimpse_engine::decoder::{AudioDecoder, AudioFrameQueue};
    use glimpse_engine::demuxer::{demux_file, PacketQueues};
    use glimpse_engine::Serial;

    let audio_track = tracks
        .audio_tracks
        .iter()
        .find(|track| Some(track.id) == tracks.active_audio)
        .or_else(|| tracks.audio_tracks.iter().find(|track| track.is_default))
        .or_else(|| tracks.audio_tracks.first())
        .ok_or_else(|| "The file has no audio stream to decode".to_string())?;
    let serial = Serial(0);
    let queues = PacketQueues::new(128, 64 * 1024 * 1024);
    let frame_queue = Arc::new(AudioFrameQueue::new(64, 32 * 1024 * 1024));
    let mut decoder = AudioDecoder::open_ffmpeg(
        file_path,
        audio_track.id.0,
        Arc::clone(&queues.audio),
        Arc::clone(&frame_queue),
    )?;

    let path = file_path.to_owned();
    let worker_tracks = tracks.clone();
    let worker_queues = queues.clone();
    let worker = thread::spawn(move || {
        demux_file(&path, &worker_tracks, &worker_queues, serial)
    });

    let mut decoded_frames = 0u64;
    let mut decoded_samples = 0u64;
    let mut first_pts = None;
    let mut last_end_pts = 0.0f64;

    loop {
        if let Err(error) = decoder.decode_step(serial) {
            queues.abort();
            let _ = worker.join();
            return Err(error);
        }

        while let Some(frame) = frame_queue.pop_timeout(Duration::from_millis(0)) {
            first_pts.get_or_insert(frame.item.pts);
            last_end_pts = frame.item.pts + frame.item.duration;
            decoded_samples += (frame.item.samples.len() / 2) as u64;
            decoded_frames += 1;
        }
        // This diagnostic decodes only audio; drain other streams so they
        // cannot fill their bounded queues and stall the demux worker.
        while queues.video.pop_timeout(Duration::from_millis(0)).is_some() {}
        while queues.subtitle.pop_timeout(Duration::from_millis(0)).is_some() {}

        if worker.is_finished() && queues.audio.stats().0 == 0 {
            break;
        }
    }

    let demux_stats = worker
        .join()
        .map_err(|_| "Demux worker panicked".to_string())?
        .map_err(|error| format!("Demux failed: {error}"))?;

    decoder.finish(serial)?;
    while let Some(frame) = frame_queue.pop_timeout(Duration::from_millis(0)) {
        first_pts.get_or_insert(frame.item.pts);
        last_end_pts = frame.item.pts + frame.item.duration;
        decoded_samples += (frame.item.samples.len() / 2) as u64;
        decoded_frames += 1;
    }

    println!(
        "Demuxed {} audio packets ({} bytes across all streams); decoded {} PCM blocks",
        demux_stats.audio_packets, demux_stats.bytes_read, decoded_frames
    );
    println!(
        "PCM output: {} samples, {} Hz, stereo f32",
        decoded_samples,
        glimpse_engine::decoder::AUDIO_OUTPUT_RATE
    );
    if let Some(first_pts) = first_pts {
        println!(
            "Decoded audio timeline: {:.3}s to {:.3}s ({:.3}s)",
            first_pts,
            last_end_pts,
            (last_end_pts - first_pts).max(0.0)
        );
    }
    if decoded_samples == 0 {
        return Err("The audio stream produced no decoded PCM samples".to_string());
    }
    Ok(())
}

fn print_tracks(label: &str, tracks: &[TrackInfo]) {
    for track in tracks {
        println!(
            "  {label} #{}: {}{}{}",
            track.id,
            track.codec_name,
            track
                .width
                .zip(track.height)
                .map(|(width, height)| format!(" {width}x{height}"))
                .unwrap_or_default(),
            track
                .language
                .as_deref()
                .map(|language| format!(" [{language}]"))
                .unwrap_or_default()
        );
    }
}
