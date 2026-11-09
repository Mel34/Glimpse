//! Audio output sink abstraction and optional output backends.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use crate::clock::MasterClock;

pub trait AudioSink: Send + Sync {
    /// Submit interleaved stereo f32 samples from a non-real-time worker.
    ///
    /// Implementations must not require callers to run on the PipeWire
    /// process thread. The timestamp is the media PTS of the first sample.
    fn push_samples(&self, samples: &[f32], pts: f64);
    fn pause(&self);
    fn resume(&self);
    fn set_volume(&self, volume: f32);
    fn get_volume(&self) -> f32;
}

pub struct MockAudioSink {
    pub clock: Arc<MasterClock>,
    pub volume: AtomicU32,
}

impl MockAudioSink {
    pub fn new(clock: Arc<MasterClock>) -> Self {
        Self {
            clock,
            volume: AtomicU32::new(1.0f32.to_bits()),
        }
    }
}

impl AudioSink for MockAudioSink {
    fn push_samples(&self, _samples: &[f32], pts: f64) {
        self.clock.set_audio_pts(pts, 0.020);
    }

    fn pause(&self) {
        self.clock.pause();
    }

    fn resume(&self) {
        self.clock.resume();
    }

    fn set_volume(&self, volume: f32) {
        self.volume
            .store(volume.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
    }

    fn get_volume(&self) -> f32 {
        f32::from_bits(self.volume.load(Ordering::Relaxed))
    }

}

#[cfg(feature = "audio-pipewire")]
mod pipewire;

#[cfg(feature = "audio-pipewire")]
pub use pipewire::{PipeWireAudioSink, PipeWireAudioSinkStats};
