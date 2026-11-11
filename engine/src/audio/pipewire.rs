//! Native PipeWire output with a real-time-safe SPSC PCM handoff.
//!
//! Decoding, resampling, and producer-side queueing happen outside PipeWire's
//! process callback. The callback only dequeues a PipeWire buffer, reads from
//! the lock-free SPSC ring, applies an atomic volume value, and copies samples.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use pipewire as pw;
use pw::spa::pod::Pod;
use rtrb::{Consumer, Producer, RingBuffer};

use super::AudioSink;
use crate::decoder::{AUDIO_OUTPUT_CHANNELS, AUDIO_OUTPUT_RATE};

const PCM_RING_CAPACITY_FRAMES: usize = AUDIO_OUTPUT_RATE as usize * 2;
const STREAM_UNCONNECTED: u8 = 0;
const STREAM_CONNECTING: u8 = 1;
const STREAM_PAUSED: u8 = 2;
const STREAM_STREAMING: u8 = 3;
const STREAM_ERROR: u8 = 4;


struct SharedState {
    paused: AtomicBool,
    volume: AtomicU32,
    dropped_samples: AtomicU64,
    underflow_samples: AtomicU64,
    eof_silence_samples: AtomicU64,
    end_of_stream: AtomicBool,
    last_requested_frames: AtomicU64,
    graph_ticks: AtomicU64,
    device_delay_ms: AtomicU64,
    resampler_buffered_frames: AtomicU64,
    time_query_failures: AtomicU64,
    stream_state: AtomicU8,
    speed: Arc<AtomicU32>,
}

impl SharedState {
    fn new(speed: Arc<AtomicU32>) -> Self {
        Self {
            paused: AtomicBool::new(false),
            volume: AtomicU32::new(1.0f32.to_bits()),
            dropped_samples: AtomicU64::new(0),
            underflow_samples: AtomicU64::new(0),
            eof_silence_samples: AtomicU64::new(0),
            end_of_stream: AtomicBool::new(false),
            last_requested_frames: AtomicU64::new(0),
            graph_ticks: AtomicU64::new(0),
            device_delay_ms: AtomicU64::new(0.0f64.to_bits()),
            resampler_buffered_frames: AtomicU64::new(0),
            time_query_failures: AtomicU64::new(0),
            stream_state: AtomicU8::new(STREAM_UNCONNECTED),
            speed,
        }
    }
}

#[derive(Default)]
struct ResamplerState {
    current: Option<[f32; AUDIO_OUTPUT_CHANNELS]>,
    next: Option<[f32; AUDIO_OUTPUT_CHANNELS]>,
    phase: f64,
    eof_last_sample_output: bool,
}

struct ProcessData {
    consumer: Consumer<[f32; AUDIO_OUTPUT_CHANNELS]>,
    shared: Arc<SharedState>,
    resampler: ResamplerState,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PipeWireAudioSinkStats {
    /// Individual f32 samples rejected because the producer ring was full.
    pub dropped_samples: u64,
    /// Individual f32 samples rendered as silence while media was still active.
    pub underflow_samples: u64,
    /// Individual f32 samples rendered as silence after the producer reached EOF.
    pub eof_silence_samples: u64,
    /// Frame count from the most recent PipeWire playback request.
    pub last_requested_frames: u64,
    /// Most recent PipeWire graph position in ticks.
    pub graph_ticks: u64,
    /// Most recent PipeWire-reported device delay, in milliseconds.
    pub device_delay_ms: f64,
    /// Frames buffered inside PipeWire's audio resampler.
    pub resampler_buffered_frames: u64,
    /// Failed real-time PipeWire timing queries.
    pub time_query_failures: u64,
    /// Whether PipeWire has negotiated the stream and allocated buffers.
    pub connected: bool,
}

pub struct PipeWireAudioSink {
    producer: Mutex<Producer<[f32; AUDIO_OUTPUT_CHANNELS]>>,
    shared: Arc<SharedState>,
    stop_sender: mpsc::Sender<()>,
    worker: Option<JoinHandle<()>>,
}

impl PipeWireAudioSink {
    /// Connects a native PipeWire playback stream at 48 kHz stereo F32LE.
    ///
    /// The PipeWire loop and stream are owned by a dedicated control thread.
    /// The process callback runs on PipeWire's real-time thread.
    pub fn new() -> Result<Self, String> {
        Self::new_with_speed(Arc::new(AtomicU32::new(1.0f32.to_bits())))
    }

    pub fn new_with_speed(speed: Arc<AtomicU32>) -> Result<Self, String> {
        let (producer, consumer) = RingBuffer::new(PCM_RING_CAPACITY_FRAMES);
        let shared = Arc::new(SharedState::new(speed));
        let worker_shared = Arc::clone(&shared);
        let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
        let (stop_sender, stop_receiver) = mpsc::channel();

        let worker_ready_sender = ready_sender.clone();
        let worker = thread::Builder::new()
            .name("glimpse-pipewire".to_string())
            .spawn(move || {
                let result = run_pipewire_thread(
                    consumer,
                    worker_shared,
                    ready_sender,
                    stop_receiver,
                );
                if let Err(error) = result {
                    let _ = worker_ready_sender.send(Err(error));
                }
            })
            .map_err(|error| format!("Could not start PipeWire control thread: {error}"))?;

        match ready_receiver.recv_timeout(Duration::from_secs(6)) {
            Ok(Ok(())) => Ok(Self {
                producer: Mutex::new(producer),
                shared,
                stop_sender,
                worker: Some(worker),
            }),
            Ok(Err(error)) => {
                let _ = stop_sender.send(());
                let _ = worker.join();
                Err(error)
            }
            Err(RecvTimeoutError::Timeout) => {
                let _ = stop_sender.send(());
                let _ = worker.join();
                Err("Timed out waiting for PipeWire playback stream connection".to_string())
            }
            Err(RecvTimeoutError::Disconnected) => {
                let _ = stop_sender.send(());
                let _ = worker.join();
                Err("PipeWire control thread exited before connecting".to_string())
            }
        }
    }

    /// Number of interleaved stereo sample frames currently queued for playback.
    ///
    /// This is a producer-side diagnostic used for non-real-time buffer pacing.
    pub fn buffered_sample_frames(&self) -> usize {
        let producer = self.producer.lock();
        PCM_RING_CAPACITY_FRAMES - producer.slots()
    }

    /// Marks that no more PCM will be submitted, so subsequent empty-ring
    /// callbacks are expected end-of-stream silence rather than starvation.
    pub fn mark_end_of_stream(&self) {
        self.shared.end_of_stream.store(true, Ordering::Release);
    }

    pub fn stats(&self) -> PipeWireAudioSinkStats {
        let state = self.shared.stream_state.load(Ordering::Acquire);
        PipeWireAudioSinkStats {
            dropped_samples: self.shared.dropped_samples.load(Ordering::Relaxed),
            underflow_samples: self.shared.underflow_samples.load(Ordering::Relaxed),
            eof_silence_samples: self.shared.eof_silence_samples.load(Ordering::Relaxed),
            last_requested_frames: self.shared.last_requested_frames.load(Ordering::Relaxed),
            graph_ticks: self.shared.graph_ticks.load(Ordering::Relaxed),
            device_delay_ms: f64::from_bits(self.shared.device_delay_ms.load(Ordering::Relaxed)),
            resampler_buffered_frames: self.shared.resampler_buffered_frames.load(Ordering::Relaxed),
            time_query_failures: self.shared.time_query_failures.load(Ordering::Relaxed),
            connected: state == STREAM_PAUSED || state == STREAM_STREAMING,
        }
    }
}

impl AudioSink for PipeWireAudioSink {
    fn push_samples(&self, samples: &[f32], _pts: f64) {
        let sample_count = samples.len() & !1;
        if sample_count == 0 {
            return;
        }

        let mut producer = self.producer.lock();
        let mut dropped = 0u64;
        for frame in samples[..sample_count].chunks_exact(AUDIO_OUTPUT_CHANNELS) {
            let stereo = [frame[0], frame[1]];
            if producer.push(stereo).is_err() {
                dropped += AUDIO_OUTPUT_CHANNELS as u64;
            }
        }
        if dropped != 0 {
            self.shared
                .dropped_samples
                .fetch_add(dropped, Ordering::Relaxed);
        }
    }

    fn pause(&self) {
        // The callback emits silence while paused without consuming queued PCM.
        self.shared.paused.store(true, Ordering::Release);
    }

    fn resume(&self) {
        self.shared.paused.store(false, Ordering::Release);
    }

    fn set_volume(&self, volume: f32) {
        let volume = if volume.is_finite() {
            volume.clamp(0.0, 1.5)
        } else {
            1.0
        };
        self.shared
            .volume
            .store(volume.to_bits(), Ordering::Relaxed);
    }

    fn get_volume(&self) -> f32 {
        f32::from_bits(self.shared.volume.load(Ordering::Relaxed))
    }
}

impl Drop for PipeWireAudioSink {
    fn drop(&mut self) {
        let _ = self.stop_sender.send(());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn run_pipewire_thread(
    consumer: Consumer<f32>,
    shared: Arc<SharedState>,
    ready_sender: SyncSender<Result<(), String>>,
    stop_receiver: Receiver<()>,
) -> Result<(), String> {
    pw::init();

    // The PipeWire wrapper marks ThreadLoopBox::new unsafe because it owns a
    // native loop pointer. This call creates a fresh loop and keeps it on this
    // thread for its entire lifetime.
    let thread_loop = unsafe {
        pw::thread_loop::ThreadLoopBox::new(Some("glimpse-audio"), None)
    }
    .map_err(|error| format!("Could not create PipeWire thread loop: {error}"))?;
    let context = pw::context::ContextBox::new(thread_loop.loop_(), None)
        .map_err(|error| format!("Could not create PipeWire context: {error}"))?;
    let core = context
        .connect(None)
        .map_err(|error| format!("Could not connect to the PipeWire server: {error}"))?;
    let stream = pw::stream::StreamBox::new(
        &core,
        "glimpse-audio",
        pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Audio",
            *pw::keys::MEDIA_CATEGORY => "Playback",
            *pw::keys::MEDIA_ROLE => "Music",
            *pw::keys::AUDIO_CHANNELS => "2",
        },
    )
    .map_err(|error| format!("Could not create PipeWire playback stream: {error}"))?;

    let process_data = ProcessData {
        consumer,
        shared: Arc::clone(&shared),
        resampler: ResamplerState::default(),
    };
    let _listener = stream
        .add_local_listener_with_user_data(process_data)
        .process(|stream, process_data| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };

            let requested_frames = usize::try_from(buffer.requested()).unwrap_or(usize::MAX);
            process_data.shared.last_requested_frames.store(
                requested_frames as u64,
                Ordering::Relaxed,
            );

            match stream.time() {
                Ok(time) => {
                    let rate = time.rate();
                    let delay_ms = if rate.denom == 0 {
                        0.0
                    } else {
                        time.delay() as f64 * rate.num as f64 * 1_000.0 / rate.denom as f64
                    };
                    process_data.shared.graph_ticks.store(time.ticks(), Ordering::Relaxed);
                    process_data.shared.device_delay_ms.store(delay_ms.to_bits(), Ordering::Relaxed);
                    process_data.shared.resampler_buffered_frames.store(
                        time.buffered(),
                        Ordering::Relaxed,
                    );
                }
                Err(_) => {
                    process_data.shared.time_query_failures.fetch_add(1, Ordering::Relaxed);
                }
            }

            let datas = buffer.datas_mut();
            let Some(data) = datas.first_mut() else {
                return;
            };

            let written_bytes = if let Some(bytes) = data.data() {
                let (written, underflow, eof_silence) = render_pcm(
                    &mut process_data.consumer,
                    &process_data.shared,
                    &mut process_data.resampler,
                    bytes,
                    requested_frames,
                );
                if underflow != 0 {
                    process_data
                        .shared
                        .underflow_samples
                        .fetch_add(underflow as u64, Ordering::Relaxed);
                }
                if eof_silence != 0 {
                    process_data
                        .shared
                        .eof_silence_samples
                        .fetch_add(eof_silence as u64, Ordering::Relaxed);
                }
                written
            } else {
                0
            };

            let chunk = data.chunk_mut();
            *chunk.offset_mut() = 0;
            *chunk.stride_mut() =
                (AUDIO_OUTPUT_CHANNELS * std::mem::size_of::<f32>()) as _;
            *chunk.size_mut() = written_bytes as _;
        })
        .register()
        .map_err(|error| format!("Could not register PipeWire stream callbacks: {error}"))?;

    let mut audio_info = pw::spa::param::audio::AudioInfoRaw::new();
    audio_info.set_format(pw::spa::param::audio::AudioFormat::F32LE);
    audio_info.set_rate(AUDIO_OUTPUT_RATE);
    audio_info.set_channels(AUDIO_OUTPUT_CHANNELS as u32);
    let mut position = [0; pw::spa::param::audio::MAX_CHANNELS];
    position[0] = pw::spa::sys::SPA_AUDIO_CHANNEL_FL;
    position[1] = pw::spa::sys::SPA_AUDIO_CHANNEL_FR;
    audio_info.set_position(position);

    let pod_bytes = pw::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &pw::spa::pod::Value::Object(pw::spa::pod::Object {
            type_: pw::spa::sys::SPA_TYPE_OBJECT_Format,
            id: pw::spa::sys::SPA_PARAM_EnumFormat,
            properties: audio_info.into(),
        }),
    )
    .map_err(|error| format!("Could not serialise PipeWire audio format: {error}"))?
    .0
    .into_inner();
    let pod = Pod::from_bytes(&pod_bytes)
        .ok_or_else(|| "Could not construct PipeWire audio format POD".to_string())?;
    let mut params = [pod];

    thread_loop.start();
    let connect_result = {
        let _guard = thread_loop.lock();
        stream.connect(
            pw::spa::utils::Direction::Output,
            None,
            pw::stream::StreamFlags::AUTOCONNECT
                | pw::stream::StreamFlags::MAP_BUFFERS
                | pw::stream::StreamFlags::RT_PROCESS,
            &mut params,
        )
    };
    if let Err(error) = connect_result {
        thread_loop.stop();
        return Err(format!("Could not connect PipeWire playback stream: {error}"));
    }

    let startup_deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let state = {
            let _guard = thread_loop.lock();
            stream.state()
        };
        shared.stream_state.store(stream_state_code(&state), Ordering::Release);

        match state {
            pw::stream::StreamState::Paused | pw::stream::StreamState::Streaming => {
                let _ = ready_sender.send(Ok(()));
                break;
            }
            pw::stream::StreamState::Error(error) => {
                thread_loop.stop();
                return Err(format!("PipeWire playback stream entered an error state: {error}"));
            }
            _ if Instant::now() >= startup_deadline => {
                thread_loop.stop();
                return Err("Timed out negotiating the PipeWire playback stream".to_string());
            }
            _ => thread::sleep(Duration::from_millis(5)),
        }
    }

    loop {
        match stop_receiver.recv_timeout(Duration::from_millis(100)) {
            Ok(()) | Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => {
                let state = {
                    let _guard = thread_loop.lock();
                    stream.state()
                };
                shared.stream_state.store(stream_state_code(&state), Ordering::Release);
                if matches!(&state, pw::stream::StreamState::Error(_))
                    || state == pw::stream::StreamState::Unconnected
                {
                    break;
                }
            }
        }
    }

    {
        let _guard = thread_loop.lock();
        let _ = stream.disconnect();
    }
    thread_loop.stop();
    Ok(())
}

fn stream_state_code(state: &pw::stream::StreamState) -> u8 {
    match state {
        pw::stream::StreamState::Unconnected => STREAM_UNCONNECTED,
        pw::stream::StreamState::Connecting => STREAM_CONNECTING,
        pw::stream::StreamState::Paused => STREAM_PAUSED,
        pw::stream::StreamState::Streaming => STREAM_STREAMING,
        pw::stream::StreamState::Error(_) => STREAM_ERROR,
    }
}

/// Fills one PipeWire F32LE buffer. This function is deliberately independent
/// of PipeWire objects so its bounded, allocation-free behavior can be tested.
fn render_pcm(
    consumer: &mut Consumer<[f32; AUDIO_OUTPUT_CHANNELS]>,
    shared: &SharedState,
    resampler: &mut ResamplerState,
    output: &mut [u8],
    requested_frames: usize,
) -> (usize, usize, usize) {
    let bytes_per_frame = AUDIO_OUTPUT_CHANNELS * std::mem::size_of::<f32>();
    let frame_capacity = output.len() / bytes_per_frame;
    let frame_count = if requested_frames == 0 {
        frame_capacity
    } else {
        requested_frames.min(frame_capacity)
    };
    let written_bytes = frame_count * bytes_per_frame;
    output[..written_bytes].fill(0);
    if frame_count == 0 || shared.paused.load(Ordering::Acquire) {
        return (written_bytes, 0, 0);
    }

    let end_of_stream = shared.end_of_stream.load(Ordering::Acquire);
    let volume = f32::from_bits(shared.volume.load(Ordering::Relaxed));
    let speed = f32::from_bits(shared.speed.load(Ordering::Acquire));
    let speed = if speed.is_finite() { speed.clamp(0.5, 32.0) as f64 } else { 1.0 };
    let mut underflow_samples = 0usize;
    let mut eof_silence_samples = 0usize;

    for output_index in 0..frame_count {
        if resampler.current.is_none() {
            resampler.current = consumer.pop().ok();
            resampler.phase = 0.0;
        }

        // Advance through source frames according to media-time playback speed.
        // This happens in the PipeWire callback, so changing speed affects PCM
        // already queued in the ring instead of leaving stale pre-resampled audio.
        while resampler.phase >= 1.0 {
            let next = resampler.next.take().or_else(|| consumer.pop().ok());
            let Some(next) = next else {
                break;
            };
            resampler.current = Some(next);
            resampler.phase -= 1.0;
        }

        if resampler.next.is_none() {
            resampler.next = consumer.pop().ok();
            if resampler.next.is_none() && end_of_stream && !resampler.eof_last_sample_output {
                resampler.next = resampler.current;
                resampler.eof_last_sample_output = resampler.next.is_some();
            }
        }

        let (Some(current), Some(next)) = (resampler.current, resampler.next) else {
            let remaining_samples = (frame_count - output_index) * AUDIO_OUTPUT_CHANNELS;
            if end_of_stream {
                eof_silence_samples = remaining_samples;
            } else {
                underflow_samples = remaining_samples;
            }
            break;
        };
        if resampler.phase >= 1.0 {
            let remaining_samples = (frame_count - output_index) * AUDIO_OUTPUT_CHANNELS;
            if end_of_stream {
                eof_silence_samples = remaining_samples;
            } else {
                underflow_samples = remaining_samples;
            }
            break;
        }

        let fraction = resampler.phase as f32;
        let left = (current[0] + (next[0] - current[0]) * fraction) * volume;
        let right = (current[1] + (next[1] - current[1]) * fraction) * volume;
        let byte_offset = output_index * bytes_per_frame;
        output[byte_offset..byte_offset + 4].copy_from_slice(&left.to_le_bytes());
        output[byte_offset + 4..byte_offset + 8].copy_from_slice(&right.to_le_bytes());
        resampler.phase += speed;
    }

    (written_bytes, underflow_samples, eof_silence_samples)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shared_state() -> SharedState {
        SharedState::new(Arc::new(AtomicU32::new(1.0f32.to_bits())))
    }

    fn push_frames(producer: &mut Producer<[f32; AUDIO_OUTPUT_CHANNELS]>, frames: &[[f32; 2]]) {
        for frame in frames {
            producer.push(*frame).unwrap();
        }
    }

    fn sample(output: &[u8], frame: usize, channel: usize) -> f32 {
        let offset = (frame * AUDIO_OUTPUT_CHANNELS + channel) * 4;
        f32::from_le_bytes(output[offset..offset + 4].try_into().unwrap())
    }

    #[test]
    fn renders_interpolated_stereo_samples_and_counts_underflow() {
        let (mut producer, mut consumer) = RingBuffer::new(8);
        let shared = shared_state();
        let mut resampler = ResamplerState::default();
        push_frames(&mut producer, &[[0.25, -0.5], [0.5, -1.0], [0.75, -1.5]]);

        let mut output = [0xff; 24];
        let (written, underflow, _) =
            render_pcm(&mut consumer, &shared, &mut resampler, &mut output, 3);

        assert_eq!(written, 24);
        assert_eq!(underflow, 2);
        assert_eq!(sample(&output, 0, 0), 0.25);
        assert_eq!(sample(&output, 0, 1), -0.5);
        assert_eq!(sample(&output, 1, 0), 0.5);
        assert_eq!(sample(&output, 1, 1), -1.0);
        assert_eq!(sample(&output, 2, 0), 0.0);
        assert_eq!(sample(&output, 2, 1), 0.0);
    }

    #[test]
    fn applies_volume_amplification_above_unity() {
        let (mut producer, mut consumer) = RingBuffer::new(8);
        let shared = shared_state();
        let mut resampler = ResamplerState::default();
        shared.volume.store(1.5f32.to_bits(), Ordering::Relaxed);
        push_frames(&mut producer, &[[0.75, -0.5], [0.5, -0.25]]);

        let mut output = [0xff; 8];
        let (written, underflow, _) =
            render_pcm(&mut consumer, &shared, &mut resampler, &mut output, 1);

        assert_eq!(written, 8);
        assert_eq!(underflow, 0);
        assert!((sample(&output, 0, 0) - 1.125).abs() < f32::EPSILON);
        assert_eq!(sample(&output, 0, 1), -0.75);
    }

    #[test]
    fn eof_silence_is_counted_separately_from_active_starvation() {
        let (mut producer, mut consumer) = RingBuffer::new(8);
        let shared = shared_state();
        let mut resampler = ResamplerState::default();
        push_frames(&mut producer, &[[0.25, -0.5], [0.5, -1.0]]);
        shared.end_of_stream.store(true, Ordering::Release);

        let mut output = [0xff; 24];
        let (written, underflow, eof_silence) =
            render_pcm(&mut consumer, &shared, &mut resampler, &mut output, 3);

        assert_eq!(written, 24);
        assert_eq!(underflow, 0);
        assert_eq!(eof_silence, 2);
        assert_eq!(sample(&output, 0, 0), 0.25);
        assert_eq!(sample(&output, 1, 0), 0.5);
        assert_eq!(sample(&output, 2, 0), 0.0);
    }

    #[test]
    fn renders_only_requested_frames_and_preserves_remaining_pcm() {
        let (mut producer, mut consumer) = RingBuffer::new(8);
        let shared = shared_state();
        let mut resampler = ResamplerState::default();
        push_frames(&mut producer, &[[0.1, 0.2], [0.3, 0.4], [0.5, 0.6], [0.7, 0.8]]);

        let mut output = [0xff; 16];
        let (written, underflow, _) =
            render_pcm(&mut consumer, &shared, &mut resampler, &mut output, 1);

        assert_eq!(written, 8);
        assert_eq!(underflow, 0);
        assert_eq!(sample(&output, 0, 0), 0.1);
        assert_eq!(sample(&output, 0, 1), 0.2);
        assert_eq!(&output[8..], &[0xff; 8]);
        assert_eq!(consumer.slots(), 2);
    }

    #[test]
    fn pause_outputs_silence_without_consuming_queued_samples() {
        let (mut producer, mut consumer) = RingBuffer::new(8);
        let shared = shared_state();
        let mut resampler = ResamplerState::default();
        push_frames(&mut producer, &[[0.5, 0.5]]);
        shared.paused.store(true, Ordering::Release);

        let mut output = [0xff; 8];
        let (written, underflow, _) =
            render_pcm(&mut consumer, &shared, &mut resampler, &mut output, 1);

        assert_eq!(written, 8);
        assert_eq!(underflow, 0);
        assert_eq!(output, [0; 8]);
        assert_eq!(consumer.slots(), 1);
    }

    #[test]
    fn playback_speed_changes_apply_to_pcm_already_in_the_ring() {
        let (mut producer, mut consumer) = RingBuffer::new(8);
        let shared = shared_state();
        let mut resampler = ResamplerState::default();
        push_frames(
            &mut producer,
            &[[0.0, 0.0], [1.0, 1.0], [2.0, 2.0], [3.0, 3.0], [4.0, 4.0]],
        );

        shared.speed.store(2.0f32.to_bits(), Ordering::Release);
        let mut first = [0; 8];
        render_pcm(&mut consumer, &shared, &mut resampler, &mut first, 1);
        assert_eq!(sample(&first, 0, 0), 0.0);

        shared.speed.store(1.0f32.to_bits(), Ordering::Release);
        let mut second = [0; 8];
        render_pcm(&mut consumer, &shared, &mut resampler, &mut second, 1);
        assert_eq!(sample(&second, 0, 0), 2.0);
    }
}
