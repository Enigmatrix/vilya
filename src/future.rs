use std::{
    future::Future,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll, Waker},
};

use io_uring::squeue::Entry;
use parking_lot::{Mutex, MutexGuard};
use std::io::{Error, Result};

pub enum CompletionState {
    Unsubmitted,
    InProgress { waker: Waker },
    Completed { result: i32 },
}

pub struct CompletionRef {
    inner: Arc<Mutex<CompletionState>>,
}

impl CompletionRef {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(CompletionState::Unsubmitted)),
        }
    }

    pub unsafe fn clone_as_user_data(&self) -> u64 {
        Arc::into_raw(Arc::clone(&self.inner)) as u64
    }

    pub unsafe fn from_user_data(user_data: u64) -> Self {
        Self {
            inner: Arc::from_raw(user_data as *const Mutex<CompletionState>),
        }
    }

    pub fn try_lock(&self) -> Option<MutexGuard<'_, CompletionState>> {
        self.inner.try_lock()
    }

    pub fn lock(&self) -> MutexGuard<'_, CompletionState> {
        self.inner.lock()
    }
}

pub trait UringOp {
    type Output;

    unsafe fn build_entry(&self, entry: &mut Entry);
    fn to_output(&self, result: i32) -> Self::Output;
}

pub struct UringFuture<T: UringOp> {
    op: T,
    state: CompletionRef,
}

impl<T: UringOp> Future for UringFuture<T> {
    type Output = Result<T::Output>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<T::Output>> {
        // To be here, we need to have been woken up by the Waker, or it's the
        // initial call to poll. Notably, we cannot be waken up and be in the
        // InProgress state.

        /*
         * If the lock is being held, then it's held only by the completion
         * handler, and that means that the current state is InProgress.
         *
         * To woken up in the a non-Unsubmitted state, that itself implies that
         * the completion handler has woken up the waker, which means that the
         * lock is not held - contradiction.
         *
         * Therefore, we can just use lock without worry, as there will never be
         * any contention.
         */
        let mut state = self.state.lock();

        match *state {
            CompletionState::Unsubmitted => {
                todo!()
            }
            CompletionState::InProgress { .. } => unreachable!(),
            CompletionState::Completed { result } => {
                if result < 0 {
                    return Poll::Ready(Err(Error::from_raw_os_error(-result)));
                }
                Poll::Ready(Ok(self.op.to_output(result)))
            }
        }
    }
}
