use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll, Waker},
};

use io_uring::squeue::Entry;
use parking_lot::{Mutex, MutexGuard};
use std::io::{Error, Result};

use crate::{owned::KernelOwned, runtime::Runtime};

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
        unsafe { self.inner.data() }.expect("data has been moved out")
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

    unsafe fn build_entry(&self, entry: &mut Entry, user_data: u64);
    fn to_output(&self, storage: Self::Storage, result: i32) -> Self::Output;
}

pub struct UringFuture<T: UringOp> {
    op: T,
    completion_ref: CompletionRef<T::Storage>,
}

impl<T: UringOp<Storage: Send + 'static>> Future for UringFuture<T> {
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
        let mut state = self.completion_ref.lock();

        match *state {
            CompletionState::Unsubmitted => {
                if Runtime::with_current(|rt| rt.is_full()) {
                    // wait to be woken
                    // TODO
                    return Poll::Pending;
                }

                let user_data = unsafe { self.completion_ref.place_clone_in_kernel() };
                let mut entry = unsafe { std::mem::zeroed::<Entry>() };
                unsafe { self.op.build_entry(&mut entry, user_data) };

                // rt.is_full() is true, so there is always space
                Runtime::with_current(|rt| rt.submit(&entry)).unwrap();

                *state = CompletionState::InProgress {
                    waker: cx.waker().clone(),
                };
                Poll::Pending
            }
            CompletionState::InProgress { .. } => unreachable!(),
            CompletionState::Completed { result } => {
                if result < 0 {
                    return Poll::Ready(Err(Error::from_raw_os_error(-result)));
                }
                let storage = self.completion_ref.data();
                Poll::Ready(Ok(self.op.to_output(storage, result)))
            }
        }
    }
}
