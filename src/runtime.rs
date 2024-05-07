use std::cell::RefCell;

use parking_lot::Mutex;

use io_uring::{
    squeue::{Entry, PushError},
    Builder, IoUring,
};

pub struct Runtime {
    uring: IoUring,
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

impl Runtime {
    pub fn new() -> Self {
        let uring = {
            let builder = IoUring::builder();
            Runtime::init(builder)
        };

        Self { uring }
    }

    pub fn is_full(&mut self) -> bool {
        self.uring.submission().is_full()
    }

    pub fn submit(&mut self, entry: &Entry) -> Result<(), PushError> {
        unsafe { self.uring.submission().push(entry) }
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
