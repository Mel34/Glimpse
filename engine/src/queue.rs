//! Bounded packet and frame queues with serial tagging for instantaneous seek invalidation.
//! Modeled on ffplay.c's PacketQueue and FrameQueue.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use parking_lot::{Condvar, Mutex};
use crate::types::Serial;

#[derive(Debug, Clone)]
pub struct TaggedItem<T> {
    pub item: T,
    pub serial: Serial,
    pub byte_size: usize,
}

pub struct BoundedQueue<T> {
    inner: Arc<Mutex<QueueState<T>>>,
    not_empty: Arc<Condvar>,
    not_full: Arc<Condvar>,
    max_count: usize,
    max_bytes: usize,
    aborted: AtomicBool,
}

struct QueueState<T> {
    items: VecDeque<TaggedItem<T>>,
    total_bytes: usize,
    serial: Serial,
}

impl<T> BoundedQueue<T> {
    pub fn new(max_count: usize, max_bytes: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(QueueState {
                items: VecDeque::with_capacity(max_count.min(256)),
                total_bytes: 0,
                serial: Serial(0),
            })),
            not_empty: Arc::new(Condvar::new()),
            not_full: Arc::new(Condvar::new()),
            max_count,
            max_bytes,
            aborted: AtomicBool::new(false),
        }
    }

    pub fn push(&self, item: T, serial: Serial, byte_size: usize) -> Result<(), ()> {
        let mut state = self.inner.lock();
        while !self.aborted.load(Ordering::Relaxed) {
            if serial < state.serial {
                return Ok(());
            }
            if state.items.len() < self.max_count && state.total_bytes + byte_size <= self.max_bytes {
                state.items.push_back(TaggedItem { item, serial, byte_size });
                state.total_bytes += byte_size;
                self.not_empty.notify_one();
                return Ok(());
            }
            self.not_full.wait(&mut state);
        }
        Err(())
    }

    pub fn pop_timeout(&self, timeout: Duration) -> Option<TaggedItem<T>> {
        let mut state = self.inner.lock();
        loop {
            if self.aborted.load(Ordering::Relaxed) {
                return None;
            }
            while let Some(front) = state.items.front() {
                if front.serial < state.serial {
                    if let Some(stale) = state.items.pop_front() {
                        state.total_bytes = state.total_bytes.saturating_sub(stale.byte_size);
                        self.not_full.notify_one();
                    }
                } else {
                    break;
                }
            }
            if let Some(valid_item) = state.items.pop_front() {
                state.total_bytes = state.total_bytes.saturating_sub(valid_item.byte_size);
                self.not_full.notify_one();
                return Some(valid_item);
            }
            let result = self.not_empty.wait_for(&mut state, timeout);
            if result.timed_out() {
                return None;
            }
        }
    }

    pub fn flush(&self, new_serial: Serial) {
        let mut state = self.inner.lock();
        state.items.clear();
        state.total_bytes = 0;
        state.serial = new_serial;
        self.not_full.notify_all();
    }

    pub fn stats(&self) -> (usize, usize, Serial) {
        let state = self.inner.lock();
        (state.items.len(), state.total_bytes, state.serial)
    }
}