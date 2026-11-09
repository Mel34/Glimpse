//! PipeWire output smoke test: plays a short 440 Hz sine tone.

use std::env;
use std::process;
use std::thread;
use std::time::Duration;

use glimpse_engine::{AudioSink, PipeWireAudioSink};

const SAMPLE_RATE: f64 = 48_000.0;
const CHUNK_FRAMES: usize = 480;

fn main() {
    let seconds = env::args()
        .nth(1)
        .and_then(|value| value.parse::<f64>().ok())
        .unwrap_or(3.0);
    if !seconds.is_finite() || seconds <= 0.0 || seconds > 30.0 {
        eprintln!("Usage: glimpse_audio_probe [duration-seconds: 0..30]");
        process::exit(2);
    }

    let sink = match PipeWireAudioSink::new() {
        Ok(sink) => sink,
        Err(error) => {
            eprintln!("Could not initialise native PipeWire output: {error}");
            process::exit(1);
        }
    };

    println!("PipeWire connected: playing a 440 Hz stereo test tone for {seconds:.1}s");
    let mut samples = vec![0.0f32; CHUNK_FRAMES * 2];
    let mut phase = 0.0f64;
    let phase_step = std::f64::consts::TAU * 440.0 / SAMPLE_RATE;
    let mut pts = 0.0f64;

    while pts < seconds {
        for frame in 0..CHUNK_FRAMES {
            let sample = (phase.sin() * 0.15) as f32;
            phase += phase_step;
            if phase >= std::f64::consts::TAU {
                phase -= std::f64::consts::TAU;
            }
            samples[frame * 2] = sample;
            samples[frame * 2 + 1] = sample;
        }

        sink.push_samples(&samples, pts);
        pts += CHUNK_FRAMES as f64 / SAMPLE_RATE;
        thread::sleep(Duration::from_millis(10));
    }

    // Allow the final queued samples to reach the device before reading stats.
    thread::sleep(Duration::from_millis(250));
    let stats = sink.stats();
    println!(
        "PipeWire output stats: connected={}, dropped={} samples, underflow={} samples",
        stats.connected,
        stats.dropped_samples,
        stats.underflow_samples
    );
    println!("If you heard the tone, native PipeWire PCM output is working.");
}
