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

    pub fn data_ref(&self) -> &T {
        unsafe { self.inner.data_ref() }.expect("data has been moved out")
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

    fn to_storage(&mut self) -> Self::Storage;
    unsafe fn build_entry(&self, entry: &mut Entry, storage: &Self::Storage, user_data: u64);
    fn to_output(&self, storage: Self::Storage, result: i32) -> Self::Output;
}

pub struct UringFuture<T: UringOp> {
    op: T,
    completion_ref: CompletionRef<T::Storage>,
}

impl<T: UringOp<Storage: Send + 'static>> UringFuture<T> {
    pub fn new(mut op: T) -> Self {
        let completion_ref = CompletionRef::new(op.to_storage());
        Self { op, completion_ref }
    }
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
                    Runtime::with_current(|rt| rt.push_waker(cx.waker().clone()));
                    return Poll::Pending;
                }

                let user_data = unsafe { self.completion_ref.place_clone_in_kernel() };
                let mut entry = unsafe { std::mem::zeroed::<Entry>() };
                unsafe {
                    self.op
                        .build_entry(&mut entry, self.completion_ref.data_ref(), user_data)
                };

                *state = CompletionState::InProgress {
                    waker: cx.waker().clone(),
                };

                // rt.is_full() is true, so there is always space
                Runtime::with_current(|rt| rt.submit(&entry)).unwrap();
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

#[cfg(test)]
mod tests {
    use awaitgroup::WaitGroup;
    use io_uring::sys::{io_uring_sqe, IORING_OP_READ};

    use super::*;
    use std::{error::Error, fs::File, os::fd::AsRawFd, sync::Arc};

    struct Read<Buf> {
        fd: i32,
        buf: Option<Buf>,
        offset: u64,
    }

    impl<Buf> Read<Buf> {
        fn new(fd: i32, buf: Buf, offset: u64) -> Self {
            Self {
                fd,
                buf: Some(buf),
                offset,
            }
        }
    }

    impl UringOp for Read<Vec<u8>> {
        type Storage = (i32, Vec<u8>);
        type Output = (Vec<u8>, usize);

        fn to_storage(&mut self) -> Self::Storage {
            (self.fd, self.buf.take().unwrap())
        }

        unsafe fn build_entry(&self, entry: &mut Entry, storage: &Self::Storage, user_data: u64) {
            let sqe = unsafe { (entry as *mut _ as *mut io_uring_sqe).as_mut().unwrap() };
            sqe.opcode = IORING_OP_READ as _;
            sqe.fd = self.fd; // TODO this could be fixed fd as well
                              // sqe.ioprio = ioprio;
            sqe.__bindgen_anon_2.addr = storage.1.as_ptr() as _;
            sqe.len = storage.1.len() as _;
            sqe.__bindgen_anon_1.off = self.offset;
            // sqe.__bindgen_anon_3.rw_flags = rw_flags;
            // sqe.__bindgen_anon_4.buf_group = buf_group;

            sqe.user_data = user_data;
        }

        fn to_output(&self, storage: Self::Storage, result: i32) -> Self::Output {
            (storage.1, result as usize)
        }
    }

    #[test]
    fn read() -> std::result::Result<(), Box<dyn Error>> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(16)
            .on_thread_park(|| {
                Runtime::with_current(|rt| rt.run_idle());
            })
            .build()?;

        let mut file = File::open("Cargo.toml").expect("open");
        let fd = file.as_raw_fd();
        let mut read_buf = vec![0; 100];
        std::io::Read::read_exact(&mut file, &mut read_buf).expect("read");
        let read_buf = Arc::new(read_buf);

        let stress = 1_000_000;
        rt.block_on(async move {
            let read_buf = read_buf.clone();
            tokio::spawn(async move {
                let read_buf = read_buf.clone();
                let mut wg = WaitGroup::new();
                (0..stress).for_each(|_| {
                    let worker = wg.worker();
                    let read_buf = read_buf.clone();
                    let fut = async move {
                        let read_buf = read_buf.clone();
                        let b = vec![0; 100];
                        let read = UringFuture::new(Read::new(fd, b, 0));
                        let (b, sz) = read.await.expect("read");

                        assert_eq!(&b[..sz], read_buf.as_ref());
                        worker.done();
                    };
                    tokio::spawn(fut);
                });
                wg.wait().await;
            })
            .await
        })?;

        Ok(())
    }
}
