use std::{
    cell::{LazyCell, RefCell},
    mem,
    sync::LazyLock,
    task::Waker,
};

use parking_lot::Mutex;

use io_uring::{
    squeue::{Entry, PushError},
    Builder, IoUring,
};

use crate::{
    future::{CompletionRef, CompletionState},
    waker_queue::SubmitWaitQueue,
};

pub struct Runtime {
    uring: IoUring,
    waker: SubmitWaitQueue,
}

thread_local! {
    pub static CURRENT_THREAD_RUNTIME: RefCell<Runtime> = RefCell::new(Runtime::new());
}

struct Initializer {
    call_count: u32,
    init: Option<Box<dyn Send + Fn(Builder) -> IoUring>>,
}

impl Initializer {
    const fn new() -> Self {
        Self {
            call_count: 0,
            init: None,
        }
    }

    fn initialize(&mut self, def_init: impl Fn(Builder) -> IoUring, builder: Builder) -> IoUring {
        self.call_count += 1;
        if let Some(ref init) = self.init {
            return init(builder);
        }
        def_init(builder)
    }

    fn set_init(&mut self, init: impl Send + Fn(Builder) -> IoUring + 'static) {
        if self.call_count != 0 {
            panic!("set_init called after more than one initialize");
        }
        self.init = Some(Box::new(init));
    }
}

static INITIALIZER: Mutex<Initializer> = Mutex::new(Initializer::new());
static WAKER: LazyLock<SubmitWaitQueue> = LazyLock::new(|| SubmitWaitQueue::new());

impl Runtime {
    pub fn new() -> Self {
        let mut uring = {
            let builder = IoUring::builder();
            Runtime::init(builder)
        };

        let waker = WAKER.clone();

        waker.add_free_entries(uring.submission().capacity() as isize);

        Self { uring, waker }
    }

    pub fn is_full(&mut self) -> bool {
        self.uring.submission().is_full()
    }

    pub fn submit(&mut self, entry: &Entry) -> Result<(), PushError> {
        self.waker.add_free_entries(-1);
        unsafe { self.uring.submission().push(entry) }
    }

    pub fn run_idle(&mut self) {
        let submitted = self.uring.submit().expect("uring submit");
        self.waker.add_free_entries(submitted as isize); // on SQPOLL, this value will be 0...
        self.reap_completions();
        self.wake();
    }

    pub fn push_waker(&self, waker: Waker) {
        self.waker.push(waker);
    }

    fn wake(&self) {
        self.waker.wake();
    }

    fn reap_completions(&mut self) -> usize {
        let mut reaped = 0;
        for cqe in self.uring.completion() {
            let result = cqe.result();
            let user_data = cqe.user_data();
            Runtime::mark_complete(user_data, result);
            reaped += 1;
        }
        return reaped;
    }

    fn mark_complete(user_data: u64, result: i32) {
        let prev_state = {
            let completion_ref = CompletionRef::<()>::from_user_data(user_data);
            let mut lock = completion_ref.lock();

            // By wake(), the completion_ref must be dropped so that the UringFuture
            // can be the sole owner if it's not already dropped. Similarly, lock must be
            // dropped so that it's not held when wake() is called.
            mem::replace(&mut *lock, CompletionState::Completed { result })
        };

        // The previous state MUST be InProgress to be even in the io-uring.
        match prev_state {
            CompletionState::InProgress { waker } => waker.wake(),
            _ => unreachable!(),
        };
    }

    pub fn with_current<T>(call: impl Fn(&mut Runtime) -> T) -> T {
        CURRENT_THREAD_RUNTIME.with_borrow_mut(call)
    }

    fn init(builder: Builder) -> IoUring {
        let mut lock = INITIALIZER.lock();
        lock.initialize(|builder| builder.build(0x400).unwrap(), builder)
    }

    /// Set a custom initializer for the IoUring used. This can only be called before
    /// any Runtime is initialized. For example, there are thread-local initializers
    /// that are called lazily, so we need to make sure this is called before
    /// any of them are used.
    pub fn set_init<F: Send + Fn(Builder) -> IoUring + 'static>(init: F) {
        let mut lock = INITIALIZER.lock();
        lock.set_init(init);
    }
}

#[cfg(test)]
mod tests {
    use std::{panic::catch_unwind, thread};

    use super::*;

    #[test]
    fn set_init_called_after_access_panics() {
        let res = thread::spawn(|| {
            Runtime::set_init(|builder| builder.build(0x100).unwrap()); // this is ok
            let _ = CURRENT_THREAD_RUNTIME.with(|_| 1);
            catch_unwind(|| {
                Runtime::set_init(|builder| builder.build(0x100).unwrap());
            })
        })
        .join()
        .unwrap();
        assert!(res.is_err())
    }
}
