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
    aborted: Arc<AtomicBool>,
}

struct QueueState<T> {
    items: VecDeque<TaggedItem<T>>,
    total_bytes: usize,
    serial: Serial,
}

impl<T> Clone for BoundedQueue<T> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
            not_empty: Arc::clone(&self.not_empty),
            not_full: Arc::clone(&self.not_full),
            max_count: self.max_count,
            max_bytes: self.max_bytes,
            aborted: Arc::clone(&self.aborted),
        }
    }
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
            max_count: max_count.max(1),
            max_bytes: max_bytes.max(1),
            aborted: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Blocks while the queue is full. A single item larger than the byte
    /// budget is accepted only when the queue is empty, then normal
    /// backpressure resumes until that item is consumed.
    pub fn push(&self, item: T, serial: Serial, byte_size: usize) -> Result<(), ()> {
        let mut state = self.inner.lock();
        while !self.aborted.load(Ordering::Acquire) {
            if serial < state.serial {
                return Ok(());
            }
            let within_budget =
                state.total_bytes.saturating_add(byte_size) <= self.max_bytes;
            if state.items.len() < self.max_count && (within_budget || state.items.is_empty()) {
                state.items.push_back(TaggedItem { item, serial, byte_size });
                state.total_bytes = state.total_bytes.saturating_add(byte_size);
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
            if self.aborted.load(Ordering::Acquire) {
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

    /// Discards queued items and advances the serial used to reject stale packets.
    pub fn flush(&self, new_serial: Serial) {
        let mut state = self.inner.lock();
        state.items.clear();
        state.total_bytes = 0;
        state.serial = new_serial;
        self.not_full.notify_all();
    }

    /// Wakes all blocked readers and writers and permanently closes the queue.
    pub fn abort(&self) {
        self.aborted.store(true, Ordering::Release);
        self.not_empty.notify_all();
        self.not_full.notify_all();
    }

    pub fn stats(&self) -> (usize, usize, Serial) {
        let state = self.inner.lock();
        (state.items.len(), state.total_bytes, state.serial)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oversized_item_is_accepted_when_queue_is_empty() {
        let queue = BoundedQueue::new(2, 8);
        queue.push("large", Serial(0), 9).unwrap();

        assert_eq!(queue.stats(), (1, 9, Serial(0)));
        assert_eq!(queue.pop_timeout(Duration::ZERO).unwrap().item, "large");
        assert_eq!(queue.stats().1, 0);
    }

    #[test]
    fn oversized_item_waits_until_existing_items_are_consumed() {
        let queue = BoundedQueue::new(2, 8);
        queue.push("small", Serial(0), 4).unwrap();

        let producer = queue.clone();
        let (sender, receiver) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            sender.send(producer.push("large", Serial(0), 9)).unwrap();
        });

        assert!(receiver.try_recv().is_err());
        assert_eq!(queue.pop_timeout(Duration::ZERO).unwrap().item, "small");
        assert!(receiver.recv_timeout(Duration::from_secs(1)).unwrap().is_ok());
        worker.join().unwrap();
        assert_eq!(queue.pop_timeout(Duration::ZERO).unwrap().item, "large");
    }

    #[test]
    fn abort_is_shared_by_queue_clones() {
        let queue = BoundedQueue::<u8>::new(1, 8);
        let producer = queue.clone();
        queue.abort();
        assert!(producer.push(1, Serial(0), 1).is_err());
        assert!(queue.pop_timeout(Duration::from_millis(1)).is_none());
    }
}
