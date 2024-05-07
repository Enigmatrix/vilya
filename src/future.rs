use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll, Waker},
};

use io_uring::squeue::Entry;
use parking_lot::{Mutex, MutexGuard};
use std::io::{Error, Result};

use crate::owned::KernelOwned;

pub enum CompletionState {
    Unsubmitted,
    InProgress { waker: Waker },
    Completed { result: i32 },
}

pub struct CompletionRef<T> {
    inner: KernelOwned<Mutex<CompletionState>, T>,
}

impl<T: Send + 'static> CompletionRef<T> {
    pub fn new(data: T) -> Self {
        Self {
            inner: KernelOwned::new(Mutex::new(CompletionState::Unsubmitted), data),
        }
    }

    pub unsafe fn place_clone_in_kernel(&self) -> u64 {
        self.inner.place_clone_in_kernel()
    }

    pub fn try_lock(&self) -> Option<MutexGuard<'_, CompletionState>> {
        self.inner.header_inner().try_lock()
    }

    pub fn lock(&self) -> MutexGuard<'_, CompletionState> {
        self.inner.header_inner().lock()
    }

    pub fn data(&self) -> T {
        let data = unsafe { self.inner.data() }.expect("data has been moved out");
        data
    }
}

impl CompletionRef<()> {
    pub fn from_user_data(user_data: u64) -> Self {
        Self {
            inner: KernelOwned::from_user_data(user_data),
        }
    }
}

pub trait UringOp {
    type Storage;
    type Output;

    unsafe fn build_entry(&self, entry: &mut Entry);
    fn to_output(&self, storage: Self::Storage, result: i32) -> Self::Output;
}

pub struct UringFuture<T: UringOp> {
    op: T,
    state: CompletionRef<T::Storage>,
}

impl<T: UringOp<Storage: Send + 'static>> Future for UringFuture<T> {
    type Output = Result<T::Output>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<T::Output>> {
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
                let storage = self.state.data();
                Poll::Ready(Ok(self.op.to_output(storage, result)))
            }
        }
    }
}
