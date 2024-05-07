use std::cell::RefCell;

use parking_lot::Mutex;

use io_uring::{Builder, IoUring};

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
        return def_init(builder);
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
