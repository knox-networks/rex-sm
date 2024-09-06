#![allow(dead_code)]

use std::{
    collections::VecDeque,
    pin::Pin,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    task::{Context, Poll, Waker},
};

use futures::stream::Stream;
use parking_lot::Mutex;

// Contains a waker for a given stream
// as well as a boolean determining whether
// stream has been woken
#[derive(Debug)]
struct ReceiverNotifier {
    handle: Waker,
    awake: Arc<AtomicBool>,
}

// holds inner VecDeque as well as the notification buffer
// letting streams know when polling is ready
// To avoid a "bowtie" effect when consuming
// objects inserted with .push_front and .push_back
// front and back values are separated into their own queue
// so that values are popped in chronological order
// irrespective of priority
// ┌ timestamp value (bigger is younger)
// ^
// │|         |
// │|||     |||
// -|||||||||||
// │|||     |||
// │|         |
// └─────|─────>
//  queue position
struct RawDeque<T> {
    front_values: VecDeque<T>,
    back_values: VecDeque<T>,
    rx_notifiers: VecDeque<ReceiverNotifier>,
}

impl<T> RawDeque<T> {
    fn new() -> Self {
        Self {
            front_values: VecDeque::new(),
            back_values: VecDeque::new(),
            rx_notifiers: VecDeque::new(),
        }
    }
}

impl<T> RawDeque<T> {
    // waker first receiver to poll for values
    fn notify_rx(&mut self) {
        if let Some(n) = self.rx_notifiers.pop_front() {
            n.handle.wake();
            n.awake.store(true, Ordering::Relaxed);
        }
    }
}

/// This type acts similarly to `std::collections::VecDeque` but
/// modifying queue is async
pub struct StreamableDeque<T> {
    inner: Mutex<RawDeque<T>>,
}

impl<T> std::fmt::Debug for StreamableDeque<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamableDeque { ... }").finish()
    }
}

impl<T> Default for StreamableDeque<T> {
    fn default() -> Self {
        Self {
            inner: Mutex::new(RawDeque::new()),
        }
    }
}

impl<T> StreamableDeque<T> {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Push an item into the queue and notify first receiver
    pub fn push_front(&self, item: T) {
        let mut inner = self.inner.lock();
        inner.front_values.push_back(item);
        // Notify first receiver in queue
        inner.notify_rx();
    }

    /// Push an item into the back of the queue and notify first receiver
    pub fn push_back(&self, item: T) {
        let mut inner = self.inner.lock();
        inner.back_values.push_back(item);
        // Notify first receiver in queue
        inner.notify_rx();
    }

    /// Returns a stream of items using `pop_front()`
    /// This opens us up to handle a `back_stream()` as well
    pub fn stream(&self) -> StreamReceiver<T> {
        StreamReceiver {
            queue: self,
            awake: None,
        }
    }

    pub fn pop_front(&self) -> Option<T> {
        let mut inner = self.inner.lock();
        inner
            .front_values
            .pop_front()
            .or_else(|| inner.back_values.pop_front())
    }

    #[cfg(test)]
    pub(crate) fn pop_back(&self) -> Option<T> {
        let mut inner = self.inner.lock();
        inner
            .back_values
            .pop_back()
            .or_else(|| inner.front_values.pop_back())
    }
}

/// A stream of items removed from the priority queue.
pub struct StreamReceiver<'a, T> {
    queue: &'a StreamableDeque<T>,
    awake: Option<Arc<AtomicBool>>,
}

impl<'a, T> Stream for StreamReceiver<'a, T> {
    type Item = T;

    fn poll_next(mut self: Pin<&mut Self>, ctx: &mut Context) -> Poll<Option<Self::Item>> {
        let mut inner = self.queue.inner.lock();

        let value = inner
            .front_values
            .pop_front()
            .or_else(|| inner.back_values.pop_front());

        if let Some(v) = value {
            self.awake = None;
            Poll::Ready(Some(v))
        } else {
            // TODO avoid allocation of a new AtomicBool if possible
            let awake = Arc::new(AtomicBool::new(false));
            // push stream's waker onto buffer
            inner.rx_notifiers.push_back(ReceiverNotifier {
                handle: ctx.waker().clone(),
                awake: awake.clone(),
            });
            self.awake = Some(awake);
            Poll::Pending
        }
    }
}

impl<'a, T> Drop for StreamReceiver<'a, T> {
    // if a stream gets dropped, notify next receiver in queue
    fn drop(&mut self) {
        let awake = self.awake.take().map(|w| w.load(Ordering::Relaxed));

        if let Some(true) = awake {
            let mut queue_wakers = self.queue.inner.lock();
            // StreamReceiver was woken by a None, notify another
            if let Some(n) = queue_wakers.rx_notifiers.pop_front() {
                n.awake.store(true, Ordering::Relaxed);
                n.handle.wake();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use futures::stream::StreamExt;

    use super::*;

    #[tokio::test]
    async fn streamable_deque() {
        let queue = Arc::new(StreamableDeque::<i32>::new());

        let pos_queue = queue.clone();
        tokio::spawn(async move {
            for i in 0..=10 {
                pos_queue.push_back(i);
            }
        });

        let neg_queue = queue.clone();
        tokio::spawn(async move {
            for i in -10..=-1 {
                neg_queue.push_front(i);
            }
        });

        let mut rx_vec = vec![];

        let mut stream = queue.stream().enumerate();
        while let Some((i, v)) = stream.next().await {
            rx_vec.push(v);
            if i >= 20 {
                break;
            }
        }

        // we should guarantee that positive and negative numbers have been pushed out of order
        // but push_front and push_back should guarantee that they are sorted
        let expected_vec: Vec<i32> = (-10..=10).collect();
        assert_eq!(expected_vec, rx_vec);
    }
}
