//! Decoder abstraction for video, audio, and subtitles.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;
use crate::clock::MasterClock;
use crate::demuxer::DemuxPacket;
use crate::presenter::VideoFrameData;
use crate::queue::BoundedQueue;
use crate::types::Serial;

pub type VideoFrameQueue = BoundedQueue<VideoFrameData>;

pub struct VideoDecoder {
    pub packet_queue: Arc<BoundedQueue<DemuxPacket>>,
    pub frame_queue: Arc<VideoFrameQueue>,
    pub clock: Arc<MasterClock>,
    pub running: Arc<AtomicBool>,
}

impl VideoDecoder {
    pub fn new(
        packet_queue: Arc<BoundedQueue<DemuxPacket>>,
        frame_queue: Arc<VideoFrameQueue>,
        clock: Arc<MasterClock>,
    ) -> Self {
        Self {
            packet_queue,
            frame_queue,
            clock,
            running: Arc::new(AtomicBool::new(true)),
        }
    }

    pub fn decode_step(&self, target_serial: Serial) -> bool {
        if let Some(tagged_packet) = self.packet_queue.pop_timeout(Duration::from_millis(20)) {
            if tagged_packet.serial != target_serial {
                return true;
            }
            let pts = tagged_packet.item.pts_secs().unwrap_or(0.0);
            let frame = VideoFrameData {
                width: 1920,
                height: 1080,
                stride: 1920 * 4,
                rgba_data: Arc::new(vec![0u8; 1920 * 1080 * 4]),
                pts,
                duration: 1.0 / 24.0,
                serial: tagged_packet.serial,
            };
            let _ = self.frame_queue.push(frame, tagged_packet.serial, 1920 * 1080 * 4);
            true
        } else {
            false
        }
    }
}