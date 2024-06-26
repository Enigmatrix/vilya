#![feature(lazy_cell)]

use std::future::Future;
use std::os::fd::RawFd;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Poll, Waker};

use crossbeam_queue::SegQueue;
use future::CompletionState;
use io_uring::squeue::Entry;
use io_uring::sys::{io_uring_sqe, IORING_OP_READ};
use owned::KernelOwned;
use parking_lot::Mutex;
use std::cell::RefCell;

mod future;
mod owned;
mod runtime;
mod waker_queue;

static CHECKER_COUNT: AtomicUsize = AtomicUsize::new(0);
static COMPLETED: AtomicUsize = AtomicUsize::new(0);

struct Checker(AtomicUsize);

impl Checker {
    fn new() -> Self {
        CHECKER_COUNT.fetch_add(1, Ordering::SeqCst);
        Self(AtomicUsize::new(1))
    }
}

impl Drop for Checker {
    fn drop(&mut self) {
        CHECKER_COUNT.fetch_sub(1, Ordering::SeqCst);
        if self.0.fetch_sub(1, Ordering::SeqCst) != 1 {
            panic!("double drop");
        }
    }
}

/// # Safety
/// This function is safe since Entry is internally a io_uring_sqe,
/// one that's private in the io_uring crate.
fn to_entry(sqe: io_uring_sqe) -> Entry {
    unsafe { std::mem::transmute(sqe) }
}

pub struct CompletionRef {
    inner: KernelOwned<Mutex<CompletionState>, (Vec<u8>, Checker)>,
}

impl CompletionRef {
    pub fn new() -> Self {
        let v = vec![0; 1];
        Self {
            inner: KernelOwned::new(
                Mutex::new(CompletionState::Unsubmitted),
                (v, Checker::new()),
            ),
        }
    }
}

pub struct Read<'a> {
    fd: i32,
    buf: &'a mut [u8],
    offset: u64,
}

pub const NR_CPU: usize = 16;

pub static INTER: AtomicUsize = AtomicUsize::new(0);

pub fn process_uring() {
    // println!("parked id={:?}", std::thread::current().id());
    RING.with(|ring| {
        let mut ring = ring.borrow_mut();

        // let len = ring.submission().len() as u32;
        // if len != 0 {
        //     let s = unsafe { ring.submitter().enter::<()>(len, 0, sys::IORING_ENTER_SQ_WAIT, None) }.expect("submit");
        // }

        ring.submit().expect("submit");
        INTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        // println!("submitted {s}");
        for cq in ring.completion() {
            let res = cq.result();
            let user_data = cq.user_data();
            // println!("CQE, result={:?}, data={:x?}", cq.result(), user_data);
            COMPLETED.fetch_add(1, Ordering::SeqCst);
            set_result(user_data, res);
        }

        // TODO expect lots of contention
        let max_size = NR_CPU * NR_CPU;
        for _ in 0..max_size {
            if let Some(waker) = WAITING_WAKERS.pop() {
                waker.wake();
            } else {
                break;
            }
        }
    });
    // println!("exiting park...")
}

// call this for each cqe received
pub fn set_result(user_data: u64, result: i32) {
    // drop locks before waking up the waker - https://users.rust-lang.org/t/should-locks-be-dropped-before-calling-waker-wake/53057
    let prev_state = {
        let what = KernelOwned::<Mutex<CompletionState>, ()>::from_user_data(user_data);
        let mut lock = what.header_inner().try_lock().unwrap();
        let prev_state = std::mem::replace(&mut *lock, CompletionState::Completed { result });
        prev_state
    };

    if let CompletionState::InProgress { waker } = prev_state {
        waker.wake();
    } else {
        unreachable!()
    }
}

impl<'a> Read<'a> {
    pub fn new(fd: i32, buf: &'a mut [u8], offset: u64) -> Self {
        Self { fd, buf, offset }
    }

    pub fn build(&self, user_data: u64) -> io_uring_sqe {
        let mut sqe: io_uring_sqe = Default::default();
        sqe.opcode = IORING_OP_READ as _;
        sqe.fd = self.fd; // TODO this could be fixed fd as well
                          // sqe.ioprio = ioprio;
        sqe.__bindgen_anon_2.addr = self.buf.as_ptr() as _;
        sqe.len = self.buf.len() as _;
        sqe.__bindgen_anon_1.off = self.offset;
        // sqe.__bindgen_anon_3.rw_flags = rw_flags;
        // sqe.__bindgen_anon_4.buf_group = buf_group;

        sqe.user_data = user_data;

        sqe
    }
}

struct Fut<'a> {
    inner: Read<'a>,
    // TODO builder pattern to set common fields like flags,
    // rw_flags, ioprio. Maybe a macro to insert it into Write etc as well.
    // Or atleast a common struct...
    state: CompletionRef,
}

impl<'a> Fut<'a> {
    pub fn new(inner: Read<'a>) -> Self {
        Self {
            inner,
            state: CompletionRef::new(),
        }
    }
}

impl Future for Fut<'_> {
    type Output = std::io::Result<i32>;

    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        let lock = self.state.inner.header_inner().try_lock().unwrap();

        match &*lock {
            CompletionState::Unsubmitted => {
                drop(lock);
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

                if RING.with_borrow_mut(|ring| ring.submission().is_full()) {
                    // can't push now, try again later
                    // process_uring();
                    WAITING_WAKERS.push(cx.waker().clone());

                    return Poll::Pending;
                }

                // never build twice!
                let user_data = self.state.inner.place_clone_in_kernel();
                let entry = to_entry(self.inner.build(user_data));
                RING.with_borrow_mut(|ring| unsafe { ring.submission().push(&entry) })
                    .unwrap();

                let mut lock = self.state.inner.header_inner().try_lock().unwrap();
                *lock = CompletionState::InProgress {
                    waker: cx.waker().clone(),
                };
                Poll::Pending
            }
            CompletionState::InProgress { .. } => unreachable!(),
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
pub static WAITING_WAKERS: SegQueue<Waker> = SegQueue::new();

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
    use std::{error::Error, fs::File, io::Seek, os::fd::AsRawFd, sync::Arc};

    use awaitgroup::WaitGroup;
    use futures::{stream::FuturesUnordered, StreamExt};

    use super::*;

    static SERIALIZE: Mutex<()> = Mutex::new(());

    // #[test]
    // fn basic_read() -> Result<(), Box<dyn Error>> {
    //     let mut ring = io_uring::IoUring::new(1000)?;
    //     let mut buf = [0; 1024];
    //     let file = File::open("Cargo.toml")?;
    //     let read = Read::new(file.as_raw_fd(), &mut buf, 0);
    //     let read_entry = to_entry(read.build());

    //     unsafe { ring.submission().push(&read_entry)? }
    //     ring.submit_and_wait(1)?;
    //     let entry = ring.completion().next().expect("one read entry");
    //     let result = entry.result();
    //     if result < 0 {
    //         return Err(std::io::Error::from_raw_os_error(-result as i32).into());
    //     }
    //     let buf = String::from_utf8_lossy(&buf[..result as usize]);
    //     assert!(buf.contains("vilya"), "io_uring read failed");
    //     Ok(())
    // }

    async fn read_cargo() -> Result<(), Box<dyn Send + Error>> {
        let mut buf = [0; 1024];
        let mut file = File::open("Cargo.toml").expect("open");
        let len = file.metadata().expect("metadata").len() as usize;
        let fd = file.as_raw_fd();
        let futs: FuturesUnordered<_> = (0..len)
            .map(|i| {
                let fut = async move {
                    let mut b = [0; 1];
                    let read = Fut::new(Read::new(fd, &mut b, i as u64));
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
        let _serialize = SERIALIZE.lock();
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
    fn stress_test_uring() -> Result<(), Box<dyn Error>> {
        let _serialize = SERIALIZE.lock();
        COMPLETED.store(0, Ordering::SeqCst);
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(16)
            .on_thread_park(|| {
                process_uring();
            })
            .build()?;

        // println!("starting... id={:?}", std::thread::current().id());
        // can't run async stuff in the main block_on as it's not a worker thread - so no parking!
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
                        let mut b = [0; 100];
                        let read = Fut::new(Read::new(fd, &mut b, 0));
                        let w = read.await.expect("read");

                        assert_eq!(&b[..w as usize], read_buf.as_ref());
                        worker.done();
                    };
                    tokio::spawn(fut);
                });
                wg.wait().await;
            })
            .await
        })?;
        let acc = INTER.load(std::sync::atomic::Ordering::Relaxed);
        println!("acc={:?}", acc);
        assert_eq!(stress - COMPLETED.load(Ordering::SeqCst), 0);
        assert_eq!(CHECKER_COUNT.load(Ordering::SeqCst), 0);
        Ok(())
    }
}
