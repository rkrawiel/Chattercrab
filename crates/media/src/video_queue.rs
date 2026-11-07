//! Queue of decoded video frames for the UI.
//!
//! The queue holds two pictures. A full queue refuses the new frame instead of
//! dropping one that is already waiting, so the playout clock can wait and keep
//! the stream smooth.

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

use tokio::sync::Notify;

/// Number of decoded frames the UI may have queued.
pub const VIDEO_QUEUE_CAPACITY: usize = 2;

/// A decoded RGBA8 frame.
#[derive(Debug, Clone)]
pub struct RgbaFrame {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Tightly packed RGBA8 pixels, `width * height * 4` bytes.
    pub pixels: Arc<[u8]>,
}

/// Bounded queue of decoded frames (capacity [`VIDEO_QUEUE_CAPACITY`]).
#[derive(Debug)]
pub struct VideoFrameQueue {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    frames: Mutex<VecDeque<RgbaFrame>>,
    /// Wakes [`VideoFrameQueue::push`] when a slot frees up.
    space: Condvar,
    /// Wakes [`VideoFrameQueue::next`].
    notify: Notify,
}

impl Default for VideoFrameQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl Clone for VideoFrameQueue {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl VideoFrameQueue {
    /// Creates an empty queue.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                frames: Mutex::new(VecDeque::with_capacity(VIDEO_QUEUE_CAPACITY)),
                space: Condvar::new(),
                notify: Notify::new(),
            }),
        }
    }

    /// Queues `frame`, waiting while the queue is full.
    pub fn push(&self, frame: RgbaFrame) {
        let mut frame = Some(frame);
        let mut guard = lock(&self.inner.frames);
        loop {
            if guard.len() < VIDEO_QUEUE_CAPACITY {
                if let Some(frame) = frame.take() {
                    guard.push_back(frame);
                }
                drop(guard);
                self.inner.notify.notify_one();
                return;
            }
            guard = self
                .inner
                .space
                .wait(guard)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }

    /// Queues `frame` if there is room. Returns the frame when the queue is full.
    pub fn try_push(&self, frame: RgbaFrame) -> Result<(), RgbaFrame> {
        let mut guard = lock(&self.inner.frames);
        if guard.len() >= VIDEO_QUEUE_CAPACITY {
            return Err(frame);
        }
        guard.push_back(frame);
        drop(guard);
        self.inner.notify.notify_one();
        Ok(())
    }

    /// Drops every queued frame.
    pub fn clear(&self) {
        lock(&self.inner.frames).clear();
        self.inner.space.notify_all();
    }

    /// Takes the oldest frame without waiting.
    pub fn try_pop(&self) -> Option<RgbaFrame> {
        let frame = lock(&self.inner.frames).pop_front()?;
        self.inner.space.notify_one();
        Some(frame)
    }

    /// Waits for the next frame.
    pub async fn next(&self) -> RgbaFrame {
        loop {
            let notified = self.inner.notify.notified();
            if let Some(frame) = self.try_pop() {
                return frame;
            }
            notified.await;
        }
    }
}

fn lock(frames: &Mutex<VecDeque<RgbaFrame>>) -> MutexGuard<'_, VecDeque<RgbaFrame>> {
    frames
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(width: u32) -> RgbaFrame {
        RgbaFrame {
            width,
            height: 1,
            pixels: Arc::from(vec![0u8; width as usize * 4]),
        }
    }

    #[test]
    fn full_queue_keeps_the_frames_already_waiting() {
        let q = VideoFrameQueue::new();
        assert!(q.try_push(frame(1)).is_ok());
        assert!(q.try_push(frame(2)).is_ok());
        assert!(q.try_push(frame(3)).is_err());
        assert_eq!(q.try_pop().map(|f| f.width), Some(1));
        assert_eq!(q.try_pop().map(|f| f.width), Some(2));
        assert!(q.try_pop().is_none());
    }

    #[tokio::test]
    async fn next_wakes_on_push() {
        let q = VideoFrameQueue::new();
        let q2 = q.clone();
        let waiter = tokio::spawn(async move { q2.next().await.width });
        tokio::task::yield_now().await;
        q.push(frame(7));
        assert_eq!(waiter.await.unwrap(), 7);
    }
}
