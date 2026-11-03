//! Audio output sink abstraction and cpal-based implementation.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use crate::clock::MasterClock;

pub trait AudioSink: Send + Sync {
    fn push_samples(&self, samples: &[f32], pts: f64);
    fn pause(&self);
    fn resume(&self);
    fn set_volume(&self, volume: f32);
    fn get_volume(&self) -> f32;
    fn flush(&self);
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
    fn pause(&self) { self.clock.pause(); }
    fn resume(&self) { self.clock.resume(); }
    fn set_volume(&self, volume: f32) {
        self.volume.store(volume.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
    }
    fn get_volume(&self) -> f32 {
        f32::from_bits(self.volume.load(Ordering::Relaxed))
    }
    fn flush(&self) {}
}