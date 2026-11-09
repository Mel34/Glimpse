//! Reusable FFmpeg-to-PipeWire audio playback session.
//!
//! Demuxing, decoding, prebuffering and PCM delivery run on worker threads.
//! The PipeWire process callback remains real-time safe; the session updates
//! the master clock from submitted media PTS and the sink's queued/device delay.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use crossbeam_channel::Sender;
use parking_lot::Mutex;
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::audio::{AudioSink, PipeWireAudioSink, PipeWireAudioSinkStats};
use crate::clock::MasterClock;
use crate::decoder::{
    AudioDecoder, AudioFrameData, AudioFrameQueue, AUDIO_OUTPUT_CHANNELS, AUDIO_OUTPUT_RATE,
};
use crate::demuxer::{DemuxStats, PacketQueues};
use crate::types::{Event, Serial, TrackInfo, TrackList};

const PACKET_QUEUE_BYTES: usize = 64 * 1024 * 1024;
const AUDIO_FRAME_QUEUE_BYTES: usize = 32 * 1024 * 1024;
const PREBUFFER_SECONDS: f64 = 0.20;
const OUTPUT_CHUNK_FRAMES: usize = 480;
const OUTPUT_BUFFER_TARGET_FRAMES: usize = AUDIO_OUTPUT_RATE as usize / 5;

/// Result of one complete audio-only playback session.
#[derive(Debug, Clone)]
pub struct AudioPlaybackReport {
    pub demux: DemuxStats,
    pub pipewire: PipeWireAudioSinkStats,
    pub pcm_blocks: u64,
    pub sample_frames: u64,
    pub first_pts: Option<f64>,
    pub last_end_pts: Option<f64>,
    pub master_clock_pts: f64,
}

/// Owns the audio master clock and creates cancellable playback workers.
pub struct AudioPlaybackSession {
    clock: Arc<MasterClock>,
}

#[derive(Default)]
struct PlaybackControl {
    paused: AtomicBool,
    stopped: AtomicBool,
    volume: AtomicU32,
    queues: Mutex<Option<(PacketQueues, Arc<AudioFrameQueue>)>>,
    finished: AtomicBool,
    ended: AtomicBool,
}

/// A joinable PipeWire playback worker owned by its Player.
pub struct AudioPlaybackHandle {
    control: Arc<PlaybackControl>,
    worker: Option<JoinHandle<()>>,
}

impl AudioPlaybackSession {
    pub fn new(clock: Arc<MasterClock>) -> Self {
        Self { clock }
    }

    /// Synchronous diagnostic entry point retained for the standalone probe.
    pub fn play_file(&self, path: &str) -> Result<AudioPlaybackReport, String> {
        run_file(path, Arc::clone(&self.clock))
    }

    /// Starts playback on a worker. Dropping the returned handle cancels and
    /// joins the worker, including its demux and PipeWire threads.
    pub fn spawn(
        &self,
        path: String,
        initial_volume: f32,
        events: Sender<Event>,
    ) -> Result<AudioPlaybackHandle, String> {
        self.spawn_from(path, initial_volume, Duration::ZERO, events)
    }

    pub fn spawn_from(
        &self,
        path: String,
        initial_volume: f32,
        start_position: Duration,
        events: Sender<Event>,
    ) -> Result<AudioPlaybackHandle, String> {
        let control = Arc::new(PlaybackControl::default());
        control.volume.store(sanitise_volume(initial_volume).to_bits(), Ordering::Relaxed);
        let worker_control = Arc::clone(&control);
        let clock = Arc::clone(&self.clock);
        let worker = thread::Builder::new()
            .name("glimpse-audio-session".to_string())
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    run_file_control(
                        &path,
                        Arc::clone(&clock),
                        worker_control.clone(),
                        start_position,
                    )
                }))
                .unwrap_or_else(|_| Err("Audio playback worker panicked".to_string()));

                if !worker_control.stopped.load(Ordering::Acquire) {
                    match result {
                        Ok(_) => {
                            worker_control.ended.store(true, Ordering::Release);
                            worker_control.finished.store(true, Ordering::Release);
                            let _ = events.send(Event::EndOfStream);
                        }
                        Err(error) => {
                            clock.pause();
                            worker_control.finished.store(true, Ordering::Release);
                            let _ = events.send(Event::StateChanged(crate::types::PlayerState::Paused));
                            let _ = events.send(Event::Error(error));
                        }
                    }
                } else {
                    clock.pause();
                    worker_control.finished.store(true, Ordering::Release);
                }
            })
            .map_err(|error| format!("Could not start audio session worker: {error}"))?;
        Ok(AudioPlaybackHandle { control, worker: Some(worker) })
    }
}

impl AudioPlaybackHandle {
    pub fn pause(&self) {
        self.control.paused.store(true, Ordering::Release);
    }

    pub fn resume(&self) {
        self.control.paused.store(false, Ordering::Release);
    }

    pub fn set_volume(&self, volume: f32) {
        self.control.volume.store(sanitise_volume(volume).to_bits(), Ordering::Relaxed);
    }

    /// Returns true once the worker has completed and published its result.
    pub fn is_finished(&self) -> bool {
        self.control.finished.load(Ordering::Acquire)
            || self.worker.as_ref().is_some_and(JoinHandle::is_finished)
    }

    /// Returns true when the worker completed successfully at natural EOF.
    pub fn reached_end_of_stream(&self) -> bool {
        self.control.ended.load(Ordering::Acquire)
    }

    pub fn stop(&mut self) {
        self.control.stopped.store(true, Ordering::Release);
        if let Some((queues, frames)) = self.control.queues.lock().as_ref() {
            queues.abort();
            frames.abort();
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        self.control.finished.store(true, Ordering::Release);
    }
}

impl Drop for AudioPlaybackHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

fn sanitise_volume(volume: f32) -> f32 {
    if volume.is_finite() { volume.clamp(0.0, 1.0) } else { 1.0 }
}

impl AudioPlaybackReport {
    pub fn decoded_duration(&self) -> f64 {
        self.last_end_pts
            .zip(self.first_pts)
            .map(|(end, start)| (end - start).max(0.0))
            .unwrap_or(0.0)
    }
}

/// Runs a complete audio-only session through FFmpeg and native PipeWire.
///
/// This is the engine-owned implementation used by both diagnostics and the
/// eventual player integration. Video/subtitle packets are drained for now;
/// unified audio/video demux consumption is the next pipeline milestone.
fn run_file(path: &str, clock: Arc<MasterClock>) -> Result<AudioPlaybackReport, String> {
    run_file_control(path, clock, Arc::new(PlaybackControl {
        paused: AtomicBool::new(false),
        stopped: AtomicBool::new(false),
        volume: AtomicU32::new(1.0f32.to_bits()),
        queues: Mutex::new(None),
        finished: AtomicBool::new(false),
        ended: AtomicBool::new(false),
    }), Duration::ZERO)
}

fn run_file_control(
    path: &str,
    clock: Arc<MasterClock>,
    control: Arc<PlaybackControl>,
    start_position: Duration,
) -> Result<AudioPlaybackReport, String> {
    let probe = crate::demuxer::probe_media(path)?;
    let audio_track = select_audio_track(&probe.tracks)
        .ok_or_else(|| "The media file has no audio stream".to_string())?;
    let serial = Serial(0);
    let queues = PacketQueues::new(128, PACKET_QUEUE_BYTES);
    let frame_queue = Arc::new(AudioFrameQueue::new(64, AUDIO_FRAME_QUEUE_BYTES));
    *control.queues.lock() = Some((queues.clone(), Arc::clone(&frame_queue)));
    let mut decoder = AudioDecoder::open_ffmpeg(
        path,
        audio_track.id.0,
        Arc::clone(&queues.audio),
        Arc::clone(&frame_queue),
    )?;
    decoder.discard_before_pts(start_position.as_secs_f64());

    let worker_path = path.to_owned();
    let worker_tracks = probe.tracks.clone();
    let worker_queues = queues.clone();
    let mut demux_worker = Some(thread::Builder::new()
        .name("glimpse-demux".to_string())
        .spawn(move || {
            crate::demuxer::demux_file_from(
                &worker_path,
                &worker_tracks,
                &worker_queues,
                serial,
                start_position,
            )
        })
        .map_err(|error| format!("Could not start demux worker: {error}"))?);

    let mut prebuffer = Vec::new();
    let mut prebuffered_seconds = 0.0;
    while prebuffered_seconds < PREBUFFER_SECONDS {
        if control.stopped.load(Ordering::Acquire) {
            abort_demux(&queues, &mut demux_worker);
            frame_queue.abort();
            return Err("Playback stopped".to_string());
        }
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
    if demux_finished(&demux_worker, &queues) {
        demux_stats = Some(join_demux(&mut demux_worker)?);
        decoder.finish(serial)?;
        collect_frames(&frame_queue, &mut prebuffer, &mut prebuffered_seconds);
    }
    if prebuffer.is_empty() {
        abort_demux(&queues, &mut demux_worker);
        return Err("The selected audio stream produced no PCM samples".to_string());
    }

    let sink = match PipeWireAudioSink::new() {
        Ok(sink) => Arc::new(sink),
        Err(error) => {
            abort_demux(&queues, &mut demux_worker);
            return Err(error);
        }
    };
    sink.set_volume(f32::from_bits(control.volume.load(Ordering::Relaxed)));
    sink.resume();
    clock.resume();

    let mut playback = PlaybackStats::default();
    for frame in prebuffer {
        if !output_frame(&sink, &clock, &control, frame, &mut playback) {
            abort_demux(&queues, &mut demux_worker);
            frame_queue.abort();
            sink.pause();
            clock.pause();
            return Err("Playback stopped".to_string());
        }
    }

    let playback_finished = Arc::new(AtomicBool::new(false));
    let playback_sink = Arc::clone(&sink);
    let playback_clock = Arc::clone(&clock);
    let playback_queue = Arc::clone(&frame_queue);
    let playback_finished_worker = Arc::clone(&playback_finished);
    let worker_control = Arc::clone(&control);
    let playback_worker = match thread::Builder::new()
        .name("glimpse-audio-playback".to_string())
        .spawn(move || {
            let mut stats = PlaybackStats::default();
            loop {
                if worker_control.stopped.load(Ordering::Acquire) {
                    break;
                }
                if let Some(frame) = playback_queue.pop_timeout(Duration::from_millis(20)) {
                    if !output_frame(&playback_sink, &playback_clock, &worker_control, frame.item, &mut stats) {
                        break;
                    }
                } else if playback_finished_worker.load(Ordering::Acquire)
                    && playback_queue.stats().0 == 0
                {
                    break;
                }
            }
            stats
        }) {
        Ok(worker) => worker,
        Err(error) => {
            abort_demux(&queues, &mut demux_worker);
            sink.pause();
            clock.pause();
            return Err(format!("Could not start audio playback worker: {error}"));
        }
    };

    let decode_result = if let Some(stats) = demux_stats {
        Ok(stats)
    } else {
        (|| {
            while demux_worker.is_some() && !control.stopped.load(Ordering::Acquire) {
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
            if control.stopped.load(Ordering::Acquire) {
                Err("Playback stopped".to_string())
            } else {
                Err("Demux worker stopped before audio decoding completed".to_string())
            }
        })()
    };

    if decode_result.is_err() || control.stopped.load(Ordering::Acquire) {
        abort_demux(&queues, &mut demux_worker);
        frame_queue.abort();
    }
    playback_finished.store(true, Ordering::Release);
    let worker_stats = playback_worker
        .join()
        .map_err(|_| "Audio playback worker panicked".to_string())?;
    let mut playback_stats = playback;
    playback_stats.blocks += worker_stats.blocks;
    playback_stats.sample_frames += worker_stats.sample_frames;
    if playback_stats.first_pts.is_none() {
        playback_stats.first_pts = worker_stats.first_pts;
    }
    playback_stats.last_end_pts = worker_stats.last_end_pts.or(playback_stats.last_end_pts);
    let demux = decode_result?;
    if control.stopped.load(Ordering::Acquire) {
        sink.pause();
        clock.pause();
        return Err("Playback stopped".to_string());
    }
    // The playback worker has joined, so no further PCM can be submitted.
    // Empty-ring callbacks from this point are expected EOF tail silence.
    sink.mark_end_of_stream();

    // Keep the clock tied to the device while the final PCM ring drains. Once
    // the ring is empty, wait out the reported hardware delay before freezing
    // the clock at the end of the submitted media timeline.
    let final_pts = playback_stats.last_end_pts.unwrap_or(0.0);
    let drain_deadline = Instant::now() + Duration::from_secs(2);
    while sink.buffered_sample_frames() > 0 {
        if !wait_until_active(&sink, &clock, &control) {
            return Err("Playback stopped".to_string());
        }
        update_audio_clock(&sink, &clock, final_pts);
        if Instant::now() >= drain_deadline {
            sink.pause();
            clock.pause();
            return Err("Timed out draining the final PipeWire audio buffer".to_string());
        }
        thread::sleep(Duration::from_millis(2));
    }
    let pipewire = sink.stats();
    let device_tail_secs = if pipewire.device_delay_ms.is_finite() {
        (pipewire.device_delay_ms / 1_000.0).clamp(0.0, 0.5)
    } else {
        0.0
    };
    let device_tail = Duration::from_secs_f64(device_tail_secs);
    thread::sleep(device_tail + Duration::from_millis(10));
    clock.set_audio_pts(final_pts, 0.0);
    sink.pause();
    clock.pause();

    if playback_stats.sample_frames == 0 {
        return Err("No decoded PCM samples reached the PipeWire sink".to_string());
    }

    Ok(AudioPlaybackReport {
        demux,
        pipewire,
        pcm_blocks: playback_stats.blocks,
        sample_frames: playback_stats.sample_frames,
        first_pts: playback_stats.first_pts,
        last_end_pts: playback_stats.last_end_pts,
        master_clock_pts: clock.get_pts(),
    })
}

fn select_audio_track(tracks: &TrackList) -> Option<&TrackInfo> {
    tracks.audio_tracks.iter()
        .find(|track| Some(track.id) == tracks.active_audio)
        .or_else(|| tracks.audio_tracks.iter().find(|track| track.is_default))
        .or_else(|| tracks.audio_tracks.first())
}

fn demux_finished(worker: &Option<JoinHandle<Result<DemuxStats, String>>>, queues: &PacketQueues) -> bool {
    worker.as_ref().map_or(true, JoinHandle::is_finished) && queues.audio.stats().0 == 0
}

fn join_demux(worker: &mut Option<JoinHandle<Result<DemuxStats, String>>>) -> Result<DemuxStats, String> {
    worker.take()
        .ok_or_else(|| "Demux worker was already joined".to_string())?
        .join()
        .map_err(|_| "Demux worker panicked".to_string())?
}

fn abort_demux(queues: &PacketQueues, worker: &mut Option<JoinHandle<Result<DemuxStats, String>>>) {
    queues.abort();
    if let Some(worker) = worker.take() {
        let _ = worker.join();
    }
}

fn collect_frames(queue: &AudioFrameQueue, frames: &mut Vec<AudioFrameData>, buffered_seconds: &mut f64) {
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
    if sample_frames == 0 { return 0; }
    stats.blocks += 1;
    stats.sample_frames += sample_frames as u64;
    stats.first_pts.get_or_insert(frame.pts);
    stats.last_end_pts = Some(frame.pts + frame.duration);
    sample_frames
}

fn output_frame(
    sink: &PipeWireAudioSink,
    clock: &MasterClock,
    control: &PlaybackControl,
    frame: AudioFrameData,
    stats: &mut PlaybackStats,
) -> bool {
    let samples = frame.samples.as_slice();
    let sample_frames = record_frame(stats, &frame);
    if sample_frames == 0 { return true; }

    let mut offset = 0;
    while offset < sample_frames {
        if !wait_until_active(sink, clock, control) {
            return false;
        }
        sink.set_volume(f32::from_bits(control.volume.load(Ordering::Relaxed)));
        let chunk_frames = (sample_frames - offset).min(OUTPUT_CHUNK_FRAMES);
        let start = offset * AUDIO_OUTPUT_CHANNELS;
        let end = (offset + chunk_frames) * AUDIO_OUTPUT_CHANNELS;
        let chunk_pts = frame.pts + offset as f64 / AUDIO_OUTPUT_RATE as f64;

        while sink.buffered_sample_frames() >= OUTPUT_BUFFER_TARGET_FRAMES {
            if !wait_until_active(sink, clock, control) {
                return false;
            }
            update_audio_clock(sink, clock, chunk_pts);
            thread::sleep(Duration::from_millis(2));
        }
        sink.push_samples(&samples[start..end], chunk_pts);
        offset += chunk_frames;
        update_audio_clock(sink, clock, chunk_pts + chunk_frames as f64 / AUDIO_OUTPUT_RATE as f64);
    }
    true
}

fn wait_until_active(
    sink: &PipeWireAudioSink,
    clock: &MasterClock,
    control: &PlaybackControl,
) -> bool {
    while control.paused.load(Ordering::Acquire) && !control.stopped.load(Ordering::Acquire) {
        sink.pause();
        clock.pause();
        thread::sleep(Duration::from_millis(5));
    }
    if control.stopped.load(Ordering::Acquire) {
        return false;
    }
    sink.resume();
    clock.resume();
    true
}

fn update_audio_clock(sink: &PipeWireAudioSink, clock: &MasterClock, submitted_end_pts: f64) {
    let stats = sink.stats();
    let queued_frames = sink.buffered_sample_frames() as u64
        + stats.resampler_buffered_frames;
    let queued_secs = queued_frames as f64 / AUDIO_OUTPUT_RATE as f64;
    clock.set_audio_pts(
        (submitted_end_pts - queued_secs).max(0.0),
        (stats.device_delay_ms / 1_000.0).max(0.0),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completed_worker_is_detected_even_without_a_completion_signal() {
        let worker = thread::spawn(|| {});
        while !worker.is_finished() {
            thread::yield_now();
        }

        let handle = AudioPlaybackHandle {
            control: Arc::new(PlaybackControl::default()),
            worker: Some(worker),
        };

        assert!(handle.is_finished());
        assert!(!handle.reached_end_of_stream());
    }

    #[test]
    fn natural_eof_is_distinguished_from_other_worker_completion() {
        let control = Arc::new(PlaybackControl::default());
        control.finished.store(true, Ordering::Release);
        control.ended.store(true, Ordering::Release);
        let handle = AudioPlaybackHandle {
            control,
            worker: None,
        };

        assert!(handle.is_finished());
        assert!(handle.reached_end_of_stream());
    }
}
