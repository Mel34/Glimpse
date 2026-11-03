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

const PCM_RING_CAPACITY: usize = AUDIO_OUTPUT_RATE as usize * AUDIO_OUTPUT_CHANNELS / 2;
const MAX_STALE_SAMPLES_PER_CALLBACK: usize = 8_192;
const STREAM_UNCONNECTED: u8 = 0;
const STREAM_CONNECTING: u8 = 1;
const STREAM_PAUSED: u8 = 2;
const STREAM_STREAMING: u8 = 3;
const STREAM_ERROR: u8 = 4;

#[derive(Clone, Copy)]
struct PcmSample {
    value: f32,
    generation: u32,
}

struct SharedState {
    generation: AtomicU32,
    paused: AtomicBool,
    volume: AtomicU32,
    dropped_samples: AtomicU64,
    underflow_samples: AtomicU64,
    flushed_samples: AtomicU64,
    stream_state: AtomicU8,
}

impl SharedState {
    fn new() -> Self {
        Self {
            generation: AtomicU32::new(0),
            paused: AtomicBool::new(false),
            volume: AtomicU32::new(1.0f32.to_bits()),
            dropped_samples: AtomicU64::new(0),
            underflow_samples: AtomicU64::new(0),
            flushed_samples: AtomicU64::new(0),
            stream_state: AtomicU8::new(STREAM_UNCONNECTED),
        }
    }
}

struct ProcessData {
    consumer: Consumer<PcmSample>,
    shared: Arc<SharedState>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PipeWireAudioSinkStats {
    /// Individual f32 samples rejected because the producer ring was full.
    pub dropped_samples: u64,
    /// Individual f32 samples rendered as silence because the ring was empty.
    pub underflow_samples: u64,
    /// Stale samples discarded after pause or flush.
    pub flushed_samples: u64,
    /// Whether PipeWire has negotiated the stream and allocated buffers.
    pub connected: bool,
}

pub struct PipeWireAudioSink {
    producer: Mutex<Producer<PcmSample>>,
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
        let (producer, consumer) = RingBuffer::new(PCM_RING_CAPACITY);
        let shared = Arc::new(SharedState::new());
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

    pub fn stats(&self) -> PipeWireAudioSinkStats {
        let state = self.shared.stream_state.load(Ordering::Acquire);
        PipeWireAudioSinkStats {
            dropped_samples: self.shared.dropped_samples.load(Ordering::Relaxed),
            underflow_samples: self.shared.underflow_samples.load(Ordering::Relaxed),
            flushed_samples: self.shared.flushed_samples.load(Ordering::Relaxed),
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

        if self.shared.paused.load(Ordering::Acquire) {
            self.shared
                .dropped_samples
                .fetch_add(sample_count as u64, Ordering::Relaxed);
            return;
        }

        let generation = self.shared.generation.load(Ordering::Acquire);
        let mut producer = self.producer.lock();
        let mut dropped = 0u64;
        for &value in &samples[..sample_count] {
            if producer.push(PcmSample { value, generation }).is_err() {
                dropped += 1;
            }
        }
        if dropped != 0 {
            self.shared
                .dropped_samples
                .fetch_add(dropped, Ordering::Relaxed);
        }
    }

    fn pause(&self) {
        self.shared.paused.store(true, Ordering::Release);
        self.flush();
    }

    fn resume(&self) {
        self.shared.paused.store(false, Ordering::Release);
    }

    fn set_volume(&self, volume: f32) {
        let volume = if volume.is_finite() {
            volume.clamp(0.0, 1.0)
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

    fn flush(&self) {
        self.shared.generation.fetch_add(1, Ordering::AcqRel);
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
    consumer: Consumer<PcmSample>,
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
    };
    let _listener = stream
        .add_local_listener_with_user_data(process_data)
        .process(|stream, process_data| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let datas = buffer.datas_mut();
            let Some(data) = datas.first_mut() else {
                return;
            };

            let written_bytes = if let Some(bytes) = data.data() {
                let (written, underflow, flushed) =
                    render_pcm(&mut process_data.consumer, &process_data.shared, bytes);
                if underflow != 0 {
                    process_data
                        .shared
                        .underflow_samples
                        .fetch_add(underflow as u64, Ordering::Relaxed);
                }
                if flushed != 0 {
                    process_data
                        .shared
                        .flushed_samples
                        .fetch_add(flushed as u64, Ordering::Relaxed);
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
        shared.stream_state.store(stream_state_code(state), Ordering::Release);

        match state {
            pw::stream::StreamState::Paused | pw::stream::StreamState::Streaming => {
                let _ = ready_sender.send(Ok(()));
                break;
            }
            pw::stream::StreamState::Error => {
                thread_loop.stop();
                return Err("PipeWire playback stream entered an error state".to_string());
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
                shared.stream_state.store(stream_state_code(state), Ordering::Release);
                if state == pw::stream::StreamState::Error
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

fn stream_state_code(state: pw::stream::StreamState) -> u8 {
    match state {
        pw::stream::StreamState::Unconnected => STREAM_UNCONNECTED,
        pw::stream::StreamState::Connecting => STREAM_CONNECTING,
        pw::stream::StreamState::Paused => STREAM_PAUSED,
        pw::stream::StreamState::Streaming => STREAM_STREAMING,
        pw::stream::StreamState::Error => STREAM_ERROR,
    }
}

/// Fills one PipeWire F32LE buffer. This function is deliberately independent
/// of PipeWire objects so its bounded, allocation-free behavior can be tested.
fn render_pcm(
    consumer: &mut Consumer<PcmSample>,
    shared: &SharedState,
    output: &mut [u8],
) -> (usize, usize, usize) {
    output.fill(0);

    let sample_count = (output.len() / std::mem::size_of::<f32>()) & !1;
    let written_bytes = sample_count * std::mem::size_of::<f32>();
    if sample_count == 0 || shared.paused.load(Ordering::Acquire) {
        return (written_bytes, 0, 0);
    }

    let generation = shared.generation.load(Ordering::Acquire);
    let volume = f32::from_bits(shared.volume.load(Ordering::Relaxed));
    let mut underflow_samples = 0usize;
    let mut flushed_samples = 0usize;
    let mut stale_limit_reached = false;

    for (index, destination) in output[..written_bytes].chunks_exact_mut(4).enumerate() {
        if stale_limit_reached {
            break;
        }

        let mut value = 0.0f32;
        loop {
            match consumer.pop() {
                Ok(sample) if sample.generation != generation => {
                    flushed_samples += 1;
                    if flushed_samples >= MAX_STALE_SAMPLES_PER_CALLBACK {
                        stale_limit_reached = true;
                        break;
                    }
                }
                Ok(sample) => {
                    value = sample.value * volume;
                    break;
                }
                Err(_) => {
                    underflow_samples = sample_count - index;
                    break;
                }
            }
        }

        if underflow_samples != 0 {
            break;
        }
        destination.copy_from_slice(&value.to_le_bytes());
    }

    (written_bytes, underflow_samples, flushed_samples)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shared_state() -> SharedState {
        SharedState::new()
    }

    #[test]
    fn renders_interleaved_float_samples_and_silences_underflow() {
        let (mut producer, mut consumer) = RingBuffer::new(8);
        let shared = shared_state();
        producer.push(PcmSample { value: 0.25, generation: 0 }).unwrap();
        producer.push(PcmSample { value: -0.5, generation: 0 }).unwrap();

        let mut output = [0xff; 16];
        let (written, underflow, flushed) = render_pcm(&mut consumer, &shared, &mut output);

        assert_eq!(written, 16);
        assert_eq!(underflow, 2);
        assert_eq!(flushed, 0);
        assert_eq!(f32::from_le_bytes(output[0..4].try_into().unwrap()), 0.25);
        assert_eq!(f32::from_le_bytes(output[4..8].try_into().unwrap()), -0.5);
        assert_eq!(&output[8..], &[0; 8]);
    }

    #[test]
    fn flush_discards_old_samples_and_preserves_new_generation() {
        let (mut producer, mut consumer) = RingBuffer::new(8);
        let shared = shared_state();
        producer.push(PcmSample { value: 0.1, generation: 0 }).unwrap();
        shared.generation.store(1, Ordering::Release);
        producer.push(PcmSample { value: 0.75, generation: 1 }).unwrap();

        let mut output = [0; 8];
        let (written, underflow, flushed) = render_pcm(&mut consumer, &shared, &mut output);

        assert_eq!(written, 8);
        assert_eq!(underflow, 0);
        assert_eq!(flushed, 1);
        assert_eq!(f32::from_le_bytes(output[0..4].try_into().unwrap()), 0.75);
        assert_eq!(f32::from_le_bytes(output[4..8].try_into().unwrap()), 0.0);
    }

    #[test]
    fn pause_outputs_silence_without_consuming_queued_samples() {
        let (mut producer, mut consumer) = RingBuffer::new(8);
        let shared = shared_state();
        producer.push(PcmSample { value: 0.5, generation: 0 }).unwrap();
        shared.paused.store(true, Ordering::Release);

        let mut output = [0xff; 8];
        let (written, underflow, flushed) = render_pcm(&mut consumer, &shared, &mut output);

        assert_eq!(written, 8);
        assert_eq!(underflow, 0);
        assert_eq!(flushed, 0);
        assert_eq!(output, [0; 8]);
        assert_eq!(consumer.slots(), 7);
    }
}
