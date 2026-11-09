//! Diagnostic client for the reusable FFmpeg-to-PipeWire audio session.

use std::env;
use std::process::ExitCode;
use std::sync::Arc;

use glimpse_engine::{audio_playback::AudioPlaybackSession, MasterClock};

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

    let session = AudioPlaybackSession::new(Arc::new(MasterClock::new()));
    match session.play_file(&path) {
        Ok(report) => {
            println!(
                "PipeWire playback: {} PCM blocks, {} sample frames, {:.3}s decoded",
                report.pcm_blocks,
                report.sample_frames,
                report.decoded_duration()
            );
            if let (Some(start), Some(end)) = (report.first_pts, report.last_end_pts) {
                println!("Audio timeline: {:.3}s to {:.3}s", start, end);
            }
            println!("Audio master clock at completion: {:.3}s", report.master_clock_pts);
            println!(
                "Demuxed {} audio packets ({} bytes across all streams)",
                report.demux.audio_packets,
                report.demux.bytes_read
            );
            println!(
                "PipeWire output stats: connected={}, dropped={} samples, underflow={} samples, flushed={} samples",
                report.pipewire.connected,
                report.pipewire.dropped_samples,
                report.pipewire.underflow_samples,
                report.pipewire.flushed_samples
            );
            println!(
                "PipeWire timing: requested={} frames, device_delay={:.3} ms, resampler_buffered={} frames, graph_ticks={}, time_query_failures={}",
                report.pipewire.last_requested_frames,
                report.pipewire.device_delay_ms,
                report.pipewire.resampler_buffered_frames,
                report.pipewire.graph_ticks,
                report.pipewire.time_query_failures
            );
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("Audio playback probe failed: {error}");
            ExitCode::FAILURE
        }
    }
}
