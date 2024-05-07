use std::{
    sync::{
        atomic::{AtomicIsize, Ordering},
        Arc,
    },
    task::Waker,
};

use crossbeam_queue::SegQueue;

pub struct SubmitWaitQueue {
    inner: Arc<SubmitWaitQueueInner>,
}
impl SubmitWaitQueue {
    pub fn new() -> Self {
        let inner = Arc::new(SubmitWaitQueueInner {
            free_entries: AtomicIsize::new(0),
            to_wake: AtomicIsize::new(0),
            nr_rings: 0,
            queue: SegQueue::new(),
        });
        Self { inner }
    }

    pub fn add_free_entries(&self, free_entries: isize) {
        self.inner.add_free_entries(free_entries);
    }

    pub fn wake(&self) {
        self.inner.wake();
    }

    pub fn push(&self, waker: Waker) {
        self.inner.push(waker);
    }
}

impl Clone for SubmitWaitQueue {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

/// Queue of Wakers to be woken when there is space in the io_uring's submission queues.
pub struct SubmitWaitQueueInner {
    free_entries: AtomicIsize, // number of free space among all the urings' submission queues (SQEs)
    to_wake: AtomicIsize,      // number of wakers to wake
    nr_rings: isize,
    queue: SegQueue<Waker>,
}

impl SubmitWaitQueueInner {
    pub fn add_free_entries(&self, free_entries: isize) {
        self.free_entries.fetch_add(free_entries, Ordering::Relaxed);
    }

    pub fn wake(&self) {
        while let Some(waker) = self.queue.pop() {
            waker.wake();

            if self.to_wake.fetch_sub(1, Ordering::Relaxed) == 0 {
                let max_wake = self
                    .free_entries
                    .load(Ordering::Relaxed)
                    .min(self.nr_rings * self.nr_rings);
                self.to_wake.store(max_wake, Ordering::Relaxed);
                break;
            }
        }
    }

    pub fn push(&self, waker: Waker) {
        self.queue.push(waker);
    }
}
