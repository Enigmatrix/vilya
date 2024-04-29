mod sys;
use std::future::Future;
use std::io::ErrorKind;
use std::ops::{Deref, DerefMut};
use std::sync::{Mutex, TryLockError};
use std::task::{Poll, Waker};
use std::{mem::zeroed, sync::Arc};
use std::borrow::BorrowMut;
use std::borrow::Borrow;

use std::cell::RefCell;
use io_uring::{opcode, squeue::Entry};
use sys::{io_uring_sqe, IORING_OP_READ};

/// # Safety
/// This function is safe since Entry is internally a io_uring_sqe,
/// one that's private in the io_uring crate.
fn to_entry(sqe: io_uring_sqe) -> Entry {
    unsafe { std::mem::transmute(sqe) }
}

pub enum CompletionState {
    Unstarted,
    InProgress(Waker),
    Completed { result: i32 },
}

pub struct CompletionRef {
    inner: Arc<Mutex<CompletionState>>,
}

impl CompletionRef {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(CompletionState::Unstarted)),
        }
    }
    
}

pub struct Read<'a> {
    fd: i32,
    buf: &'a mut [u8],
    offset: u64,
    // TODO builder pattern to set common fields like flags,
    // rw_flags, ioprio. Maybe a macro to insert it into Write etc as well.
    // Or atleast a common struct...

    state: CompletionRef,
}

// call this for each cqe received
pub fn set_result(user_data: u64, result: i32) {
    // drop locks before waking up the waker - https://users.rust-lang.org/t/should-locks-be-dropped-before-calling-waker-wake/53057
    let prev_state = {
        let state = unsafe { Arc::from_raw(user_data as *const Mutex<CompletionState>) };
        // expect very little contention
        let mut lock = state.lock().expect("mutex not poisoned");
        let prev_state = std::mem::replace(&mut *lock, CompletionState::Completed { result });
        prev_state
    };

    if let CompletionState::InProgress(waker) = prev_state {
        waker.wake();
    } else {
        unreachable!()
    }
}

impl<'a> Read<'a> {
    pub fn new(fd: i32, buf: &'a mut [u8], offset: u64) -> Self {
        Self { fd, buf, offset, state: CompletionRef::new() }
    }

    pub fn build(&self) -> io_uring_sqe {
        let mut sqe: io_uring_sqe = Default::default();
        sqe.opcode = IORING_OP_READ as _;
        sqe.fd = self.fd; // TODO this could be fixed fd as well
        // sqe.ioprio = ioprio;
        sqe.__bindgen_anon_2.addr = self.buf.as_ptr() as _;
        sqe.len = self.buf.len() as _;
        sqe.__bindgen_anon_1.off = self.offset;
        // sqe.__bindgen_anon_3.rw_flags = rw_flags;
        // sqe.__bindgen_anon_4.buf_group = buf_group;
        
        let state_clone = self.state.inner.clone();
        let state_clone_ptr = Arc::into_raw(state_clone);
        sqe.user_data = state_clone_ptr as _;

        sqe
    }
}

impl Future for Read<'_> {
    type Output = std::io::Result<i32>;

    fn poll(self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<Self::Output> {
        let mut lock = match self.state.inner.try_lock() {
            Ok(lock) => lock,
            Err(TryLockError::WouldBlock) => return Poll::Pending,
            Err(e) => panic!("mutex poisoned?")
        };
        
        match &*lock {
            CompletionState::Unstarted => {
                let entry = to_entry(self.build());
                println!("pushing entry threadid={:?}", std::thread::current().id());
                if let Err(e) = RING.with_borrow_mut(|ring| unsafe { ring.submission().push(&entry) }) {
                    return Poll::Ready(Err(std::io::Error::new(ErrorKind::Other, e)));
                }
                *lock = CompletionState::InProgress(cx.waker().clone());
                Poll::Pending
            }
            CompletionState::InProgress(_) => {
                Poll::Pending
            },
            CompletionState::Completed { result } => {
                Poll::Ready(Ok(*result))
            },
        }
    }
}

thread_local! {
    pub static RING: RefCell<io_uring::IoUring> = RefCell::new(io_uring::IoUring::new(1000).expect("io_uring"));
}

struct Never;

impl Future for Never {
    type Output = ();

    fn poll(self: std::pin::Pin<&mut Self>, _: &mut std::task::Context<'_>) -> std::task::Poll<Self::Output> {
        Poll::Pending
    }
}

// this timesout as the main block_on thread does not park
// #[test]
// fn never_test() -> Result<(), Box<dyn std::error::Error>> {
//     let rt = tokio::runtime::Builder::new_multi_thread()
//         .worker_threads(2)
//         .on_thread_park(|| {
//             println!("parking, id={:?}", std::thread::current().id());
//         })
//         .build()?;

//     println!("main thread, id={:?}", std::thread::current().id());
//     async fn wrapper<'a>(never: Never) {
//         println!("running, id={:?}", std::thread::current().id());
//         never.await;
//     }
//     rt.block_on(async move {
//         // wrappers are scheduled onto thread 3/4
//         tokio::spawn(wrapper(Never));
//         tokio::spawn(wrapper(Never));
//         tokio::spawn(wrapper(Never));
//         tokio::spawn(wrapper(Never));
//         tokio::spawn(wrapper(Never));
//         tokio::spawn(wrapper(Never));
//         tokio::spawn(wrapper(Never));
//         tokio::spawn(wrapper(Never));
//         wrapper(Never).await;
//     });
//     Ok(())
// }


#[cfg(test)]
mod tests {
    use std::{
        error::Error, fs::File, io::{Seek}, os::fd::AsRawFd
    };

    use futures::{future, stream::FuturesUnordered, StreamExt};

    use super::*;

    #[test]
    fn basic_read() -> Result<(), Box<dyn Error>> {
        let mut ring = io_uring::IoUring::new(1000)?;
        let mut buf = [0; 1024];
        let file = File::open("Cargo.toml")?;
        let read = Read::new(file.as_raw_fd(), &mut buf, 0);
        let read_entry = to_entry(read.build());
        
        unsafe { ring.submission().push(&read_entry)? }
        ring.submit_and_wait(1)?;
        let entry = ring.completion().next().expect("one read entry");
        let result = entry.result();
        if result < 0 {
            return Err(std::io::Error::from_raw_os_error(-result as i32).into());
        }
        let buf = String::from_utf8_lossy(&buf[..result as usize]);
        assert!(buf.contains("vilya"), "io_uring read failed");
        Ok(())
    }
    
    async fn read_cargo() -> Result<(), Box<dyn Send + Error>> {
        let mut buf = [0; 1024];
        let mut file = File::open("Cargo.toml").expect("open");
        let len = file.metadata().expect("metadata").len() as usize;
        let fd = file.as_raw_fd();
        let futs: FuturesUnordered<_> = (0..len).map(|i| {
            let fut = async move {
                let mut b = [0; 1];
                let read = Read::new(fd, &mut b, i as u64);
                read.await.expect("read");
                (i, b)
            };
            tokio::spawn(fut)
        }).collect();
        let res = futs.collect::<Vec<_>>().await;
        res.into_iter().for_each(|res| {
            //let (i, b) = res;
            let (i, b) = res.expect("fut");
            buf[i] = b[0];
        });

        let buf = &buf[..len as usize];
        let mut fbuf = Vec::new();
        let flen = {
            use std::io::Read;
            file.seek(std::io::SeekFrom::Start(0)).expect("seek to start");
            file.read_to_end(&mut fbuf).expect("fread")
        };
        assert_eq!(len, flen);
        assert!(buf == &fbuf[..flen], "io_uring read failed");
        Ok(())
    }
    
    #[test]
    fn lmao_test() -> Result<(), Box<dyn Error>> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(16)
            .on_thread_park(|| {
                println!("parked id={:?}", std::thread::current().id());
                RING.with(|ring| {
                    let mut ring = ring.borrow_mut();
                    let s = ring.submit().expect("submit");
                    println!("submitted {s}");
                    for cq in ring.completion() {
                        let res = cq.result();
                        let user_data = cq.user_data();
                        println!("CQE, result={:?}, data={:x?}", cq.result(), user_data);
                        set_result(user_data, res);
                    }
                });
                println!("exiting park...")
            })
            .build()?;

        println!("starting... id={:?}", std::thread::current().id());
        // can't run async stuff in the main block_on as it's not a worker thread - so no parking!
        let w= rt.block_on(async move {
            tokio::spawn(read_cargo()).await
        })?;
        w.expect("what");
        Ok(())
    }

}
