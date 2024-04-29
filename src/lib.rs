mod sys;
use std::collections::VecDeque;
use std::future::Future;
use std::os::fd::RawFd;
use std::os::fd::AsRawFd;
use std::sync::Arc;
use std::task::{Poll, Waker};

use io_uring::squeue::Entry;
use parking_lot::{Mutex, RwLock};
use std::cell::RefCell;
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

pub const NR_CPU: usize = 16;

pub fn process_uring() {
    // println!("parked id={:?}", std::thread::current().id());
    RING.with(|ring| {
        let mut ring = ring.borrow_mut();
        let s = ring.submit().expect("submit");
        // println!("submitted {s}");
        for cq in ring.completion() {
            let res = cq.result();
            let user_data = cq.user_data();
            // println!("CQE, result={:?}, data={:x?}", cq.result(), user_data);
            set_result(user_data, res);
        }

        // TODO expect lots of contention
        let mut wakers = WAITING_WAKERS.upgradable_read();
        if !wakers.is_empty() {
            wakers.try_with_upgraded(|wakers| {
                let len = (NR_CPU * NR_CPU).min(wakers.len());
                //let len = 1;
                wakers.drain(..len).for_each(|waker| waker.wake())
            });
        }
    });
    // println!("exiting park...")
}

// call this for each cqe received
pub fn set_result(user_data: u64, result: i32) {
    // drop locks before waking up the waker - https://users.rust-lang.org/t/should-locks-be-dropped-before-calling-waker-wake/53057
    let prev_state = {
        let state = unsafe { Arc::from_raw(user_data as *const Mutex<CompletionState>) };
        // expect very little contention
        let mut lock = state.lock();
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
        Self {
            fd,
            buf,
            offset,
            state: CompletionRef::new(),
        }
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

    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        let mut lock = match self.state.inner.try_lock() {
            Some(lock) => lock,
            // TODO there might be a race cond here: if the waker is woken up before we get the lock,
            // then we might miss the waker.
            None => return Poll::Pending,
        };

        match &*lock {
            CompletionState::Unstarted => {
                let entry = to_entry(self.build());
                // println!("pushing entry threadid={:?}", std::thread::current().id());
                // TODO EW blocking.... and possible deadlock since thread won't get parked like this?
                //
                // how about we hold an vector of wakers on the uring side, and if we can't push,
                // we push the waker to the vector, then return Pending. When we can process shit on the io_uring side,
                // then we wake up some wakers (just one? multiple of NR_CPU? or maybe NR_CPU*NR_CPU, or just all?)
                // This means that we need to hold a new CompletionState enum like CompletionState::Waiting to hold the
                // Entry (otherwise we might keep calling build, ew...). Note that the vector of wakers might need to be
                // global...
                // maybe we can call process_uring after a certain threshold of waiting occurs? so need to store wait count
                // in the Waiting state
                if let Err(_) =
                    RING.with_borrow_mut(|ring| unsafe { ring.submission().push(&entry) })
                {
                    // can't push now, try again later
                    // process_uring();
                    let mut wakers = WAITING_WAKERS.write();
                    wakers.push_back(cx.waker().clone());

                    return Poll::Pending;
                }
                *lock = CompletionState::InProgress(cx.waker().clone());
                Poll::Pending
            }
            CompletionState::InProgress(_) => Poll::Pending,
            CompletionState::Completed { result } => {
                if *result < 0 {
                    Poll::Ready(Err(std::io::Error::from_raw_os_error(-*result as i32)))
                } else {
                    Poll::Ready(Ok(*result))
                }
            }
        }
    }
}

thread_local! {
    pub static RING: RefCell<io_uring::IoUring> = {
        let mut builder = io_uring::IoUring::builder();
        builder.setup_single_issuer();
        builder.setup_submit_all();

        // builder.setup_iopoll();
        // let mut shared_wq = true;
        // let mut fd = unsafe { SHARED_RING_FD.lock() };
        // if let Some(ref fd) = &*fd {
        //     builder.setup_attach_wq(*fd);
        //     shared_wq = false;
        // }
        let ring = builder.build(0x4000).expect("io_uring");
        // if shared_wq {
        //     *fd = Some(ring.as_raw_fd());
        // }
        RefCell::new(ring)
    }
}

pub static mut SHARED_RING_FD: Mutex<Option<RawFd>> = Mutex::new(None);
pub static WAITING_WAKERS: RwLock<VecDeque<Waker>> = RwLock::new(VecDeque::new());

struct Never;

impl Future for Never {
    type Output = ();

    fn poll(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
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
    use std::{error::Error, fs::File, io::Seek, os::fd::AsRawFd};

    use awaitgroup::WaitGroup;
    use futures::{stream::FuturesUnordered, StreamExt};

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
        let futs: FuturesUnordered<_> = (0..len)
            .map(|i| {
                let fut = async move {
                    let mut b = [0; 1];
                    let read = Read::new(fd, &mut b, i as u64);
                    read.await.expect("read");
                    (i, b)
                };
                tokio::spawn(fut)
            })
            .collect();
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
            file.seek(std::io::SeekFrom::Start(0))
                .expect("seek to start");
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
            .on_thread_park(|| process_uring())
            .build()?;

        // println!("starting... id={:?}", std::thread::current().id());
        // can't run async stuff in the main block_on as it's not a worker thread - so no parking!
        let w = rt.block_on(async move { tokio::spawn(read_cargo()).await })?;
        w.expect("what");
        Ok(())
    }

    #[test]
    fn stress_test() -> Result<(), Box<dyn Error>> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(16)
            .on_thread_park(|| {
                process_uring();
            })
            .build()?;

        // println!("starting... id={:?}", std::thread::current().id());
        // can't run async stuff in the main block_on as it's not a worker thread - so no parking!
        let file = File::open("Cargo.toml").expect("open");
        let fd = file.as_raw_fd();
        let stress = 10_000_000;
        rt.block_on(async move {
            tokio::spawn(async move {
                let mut wg = WaitGroup::new();
                (0..stress).for_each(|_| {
                    let worker = wg.worker();
                    let fut = async move {
                        let mut b = [0; 1];
                        let read = Read::new(fd, &mut b, 0);
                        assert_eq!(1, read.await.expect("read"));
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
