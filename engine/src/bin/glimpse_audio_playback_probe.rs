//! End-to-end media audio playback probe using FFmpeg and native PipeWire output.
//!
//! This diagnostic intentionally exercises audio only. Video and subtitle packets
//! are drained so they cannot apply backpressure to the demux worker.

use std::env;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use glimpse_engine::decoder::{
    AudioDecoder, AudioFrameData, AudioFrameQueue, AUDIO_OUTPUT_CHANNELS, AUDIO_OUTPUT_RATE,
};
use glimpse_engine::demuxer::{demux_file, DemuxStats, PacketQueues};
use glimpse_engine::{AudioSink, PipeWireAudioSink, Serial, TrackList};

const PACKET_QUEUE_BYTES: usize = 64 * 1024 * 1024;
const AUDIO_FRAME_QUEUE_BYTES: usize = 32 * 1024 * 1024;
const PREBUFFER_SECONDS: f64 = 0.20;
const OUTPUT_CHUNK_FRAMES: usize = 480;

fn main() -> ExitCode {
    let mut args = env::args().skip(1);
    let Some(path) = args.next() else {
        eprintln!("Usage: glimpse_audio_playback_probe <media-file>");
        return ExitCode::from(2);
    };
    if args.next().is_some() {
        eprintln!("Usage: glimpse_audio_playback_probe <media-file>");
        return ExitCode::from(2);
    }

    match play_file(&path) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("Audio playback probe failed: {error}");
            ExitCode::FAILURE
        }
    }
}

fn play_file(path: &str) -> Result<(), String> {
    let probe = glimpse_engine::demuxer::probe_media(path)?;
    let audio_track = select_audio_track(&probe.tracks)
        .ok_or_else(|| "The media file has no audio stream".to_string())?;

    println!(
        "Selected audio stream #{}: {}",
        audio_track.id,
        audio_track.codec_name
    );

    let serial = Serial(0);
    let queues = PacketQueues::new(128, PACKET_QUEUE_BYTES);
    let frame_queue = Arc::new(AudioFrameQueue::new(64, AUDIO_FRAME_QUEUE_BYTES));
    let mut decoder = AudioDecoder::open_ffmpeg(
        path,
        audio_track.id.0,
        Arc::clone(&queues.audio),
        Arc::clone(&frame_queue),
    )?;

    let worker_path = path.to_owned();
    let worker_tracks = probe.tracks.clone();
    let worker_queues = queues.clone();
    let mut demux_worker = Some(thread::spawn(move || {
        demux_file(&worker_path, &worker_tracks, &worker_queues, serial)
    }));

    // Decode a short lead-in before connecting PipeWire. This avoids starting
    // the device stream with an empty PCM ring while FFmpeg warms up.
    let mut prebuffer = Vec::new();
    let mut prebuffered_seconds = 0.0;
    while prebuffered_seconds < PREBUFFER_SECONDS {
        if let Err(error) = decoder.decode_step(serial) {
            abort_demux(&queues, &mut demux_worker);
            return Err(error);
        }
        collect_frames(&frame_queue, &mut prebuffer, &mut prebuffered_seconds);
        drain_non_audio_packets(&queues);

        if demux_finished(&demux_worker, &queues) {
            break;
        }
    }

    let mut demux_stats = None;
    let mut decoder_finished = false;
    if demux_finished(&demux_worker, &queues) {
        demux_stats = Some(join_demux(&mut demux_worker)?);
        decoder.finish(serial)?;
        decoder_finished = true;
        collect_frames(&frame_queue, &mut prebuffer, &mut prebuffered_seconds);
    }

    if prebuffer.is_empty() {
        return Err("The selected audio stream produced no PCM samples".to_string());
    }

    let sink = match PipeWireAudioSink::new() {
        Ok(sink) => sink,
        Err(error) => {
            abort_demux(&queues, &mut demux_worker);
            return Err(error);
        }
    };

    println!(
        "PipeWire connected: playing decoded media audio ({} Hz, stereo f32)",
        AUDIO_OUTPUT_RATE
    );

    let sink = Arc::new(sink);
    let mut playback = PlaybackStats::default();
    for frame in prebuffer {
        prime_output_frame(&sink, frame, &mut playback);
    }

    // Decode and playback must run independently: packet decoding can briefly
    // stall, while the PipeWire producer needs to keep feeding PCM at 1x speed.
    let playback_finished = Arc::new(AtomicBool::new(false));
    let playback_sink = Arc::clone(&sink);
    let playback_queue = Arc::clone(&frame_queue);
    let playback_finished_worker = Arc::clone(&playback_finished);
    let playback_worker = thread::Builder::new()
        .name("glimpse-audio-playback".to_string())
        .spawn(move || {
            loop {
                if let Some(frame) = playback_queue.pop_timeout(Duration::from_millis(20)) {
                    output_frame(&playback_sink, frame.item, &mut playback);
                } else if playback_finished_worker.load(Ordering::Acquire)
                    && playback_queue.stats().0 == 0
                {
                    break;
                }
            }
            playback
        })
        .map_err(|error| format!("Could not start audio playback worker: {error}"))?;

    let decode_result = (|| {
        while demux_worker.is_some() {
            let received_packet = decoder.decode_step(serial)?;
            drain_non_audio_packets(&queues);

            if demux_finished(&demux_worker, &queues) {
                let stats = join_demux(&mut demux_worker)?;
                decoder.finish(serial)?;
                return Ok(stats);
            }

            if !received_packet {
                thread::yield_now();
            }
        }

        Err("Demux worker stopped before audio decoding completed".to_string())
    })();

    if decode_result.is_err() {
        abort_demux(&queues, &mut demux_worker);
    }
    playback_finished.store(true, Ordering::Release);
    let playback = playback_worker
        .join()
        .map_err(|_| "Audio playback worker panicked".to_string())?;
    let demux_stats = decode_result?;

    // Let the final queued samples reach the device before reporting counters.
    thread::sleep(Duration::from_millis(250));
    let output = sink.stats();
    let stats = demux_stats;
    println!(
        "Demuxed {} audio packets ({} bytes across all streams)",
        stats.audio_packets, stats.bytes_read
    );
    println!(
        "PipeWire playback: {} PCM blocks, {} sample frames, {:.3}s decoded",
        playback.blocks,
        playback.sample_frames,
        playback.last_end_pts
            .zip(playback.first_pts)
            .map(|(end, start)| (end - start).max(0.0))
            .unwrap_or(0.0)
    );
    if let Some(first_pts) = playback.first_pts {
        println!(
            "Audio timeline: {:.3}s to {:.3}s",
            first_pts,
            playback.last_end_pts.unwrap_or(first_pts)
        );
    }
    println!(
        "PipeWire output stats: connected={}, dropped={} samples, underflow={} samples, flushed={} samples",
        output.connected,
        output.dropped_samples,
        output.underflow_samples,
        output.flushed_samples
    );

    if playback.sample_frames == 0 {
        return Err("No decoded PCM samples reached the PipeWire sink".to_string());
    }
    Ok(())
}

fn select_audio_track(tracks: &TrackList) -> Option<&glimpse_engine::TrackInfo> {
    tracks
        .audio_tracks
        .iter()
        .find(|track| Some(track.id) == tracks.active_audio)
        .or_else(|| tracks.audio_tracks.iter().find(|track| track.is_default))
        .or_else(|| tracks.audio_tracks.first())
}

fn demux_finished(
    worker: &Option<JoinHandle<Result<DemuxStats, String>>>,
    queues: &PacketQueues,
) -> bool {
    worker.as_ref().map_or(true, JoinHandle::is_finished) && queues.audio.stats().0 == 0
}

fn join_demux(
    worker: &mut Option<JoinHandle<Result<DemuxStats, String>>>,
) -> Result<DemuxStats, String> {
    worker
        .take()
        .ok_or_else(|| "Demux worker was already joined".to_string())?
        .join()
        .map_err(|_| "Demux worker panicked".to_string())?
}

fn abort_demux(
    queues: &PacketQueues,
    worker: &mut Option<JoinHandle<Result<DemuxStats, String>>>,
) {
    queues.abort();
    if let Some(worker) = worker.take() {
        let _ = worker.join();
    }
}

fn collect_frames(
    queue: &AudioFrameQueue,
    frames: &mut Vec<AudioFrameData>,
    buffered_seconds: &mut f64,
) {
    while let Some(frame) = queue.pop_timeout(Duration::ZERO) {
        *buffered_seconds += frame.item.duration;
        frames.push(frame.item);
    }
}

fn drain_non_audio_packets(queues: &PacketQueues) {
    while queues.video.pop_timeout(Duration::ZERO).is_some() {}
    while queues.subtitle.pop_timeout(Duration::ZERO).is_some() {}
}

#[derive(Default)]
struct PlaybackStats {
    blocks: u64,
    sample_frames: u64,
    first_pts: Option<f64>,
    last_end_pts: Option<f64>,
}

fn record_frame(stats: &mut PlaybackStats, frame: &AudioFrameData) -> usize {
    let sample_frames = frame.samples.len() / AUDIO_OUTPUT_CHANNELS;
    if sample_frames == 0 {
        return 0;
    }

    stats.blocks += 1;
    stats.sample_frames += sample_frames as u64;
    stats.first_pts.get_or_insert(frame.pts);
    stats.last_end_pts = Some(frame.pts + frame.duration);
    sample_frames
}

// The initial lead-in is already buffered before the device starts consuming.
// Queue it in one burst so PipeWire begins with a useful amount of PCM available.
fn prime_output_frame(
    sink: &PipeWireAudioSink,
    frame: AudioFrameData,
    stats: &mut PlaybackStats,
) {
    if record_frame(stats, &frame) == 0 {
        return;
    }
    sink.push_samples(frame.samples.as_slice(), frame.pts);
}

fn output_frame(sink: &PipeWireAudioSink, frame: AudioFrameData, stats: &mut PlaybackStats) {
    let samples = frame.samples.as_slice();
    let sample_frames = record_frame(stats, &frame);
    if sample_frames == 0 {
        return;
    }

    let mut offset = 0;
    while offset < sample_frames {
        let chunk_frames = (sample_frames - offset).min(OUTPUT_CHUNK_FRAMES);
        let start = offset * AUDIO_OUTPUT_CHANNELS;
        let end = (offset + chunk_frames) * AUDIO_OUTPUT_CHANNELS;
        let chunk_pts = frame.pts + offset as f64 / AUDIO_OUTPUT_RATE as f64;
        sink.push_samples(&samples[start..end], chunk_pts);
        thread::sleep(Duration::from_secs_f64(
            chunk_frames as f64 / AUDIO_OUTPUT_RATE as f64,
        ));
        offset += chunk_frames;
    }
}
