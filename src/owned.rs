use std::sync::atomic::{self, AtomicUsize, Ordering};
use std::{mem, ptr};

#[repr(C)]
struct KernelOwnedOnHeapHeader<H> {
    ref_count: AtomicUsize,
    dealloc: fn(*mut Self),
    inner: H,
}

#[repr(C)]
struct KernelOwnedOnHeap<H, T> {
    header: KernelOwnedOnHeapHeader<H>,
    data: Option<T>,
}

impl<H, T: Send + 'static> KernelOwnedOnHeap<H, T> {
    pub fn new(header: H, data: T) -> Self {
        Self {
            header: KernelOwnedOnHeapHeader {
                ref_count: AtomicUsize::new(1),
                dealloc: |this| {
                    drop(unsafe { Box::<Self>::from_raw(this as *mut _) });
                },
                inner: header,
            },
            data: Some(data),
        }
    }
}

pub struct KernelOwned<H, T> {
    ptr: ptr::NonNull<KernelOwnedOnHeap<H, T>>,
}

unsafe impl<H, T> Send for KernelOwned<H, T> {}

impl<H, T: Send + 'static> KernelOwned<H, T> {
    pub fn new(header: H, data: T) -> Self {
        let ptr = Box::into_raw(Box::new(KernelOwnedOnHeap::new(header, data)));

        Self {
            ptr: unsafe { ptr::NonNull::new_unchecked(ptr) },
        }
    }
}

impl<H, T> KernelOwned<H, T> {
    fn header(&self) -> &KernelOwnedOnHeapHeader<H> {
        unsafe { &self.ptr.as_ref().header }
    }

    pub fn header_inner(&self) -> &H {
        &self.header().inner
    }

    /// # Safety
    /// If Some is returned, the KernelOwned instance is no longer valid.
    /// Specifically, a second call to `data` will return None. This should only
    /// be called when you are the sole owner of the KernelOwned instance.
    pub unsafe fn data(&self) -> Option<T> {
        let on_heap = self.ptr.as_ptr();
        (*on_heap).data.take()
    }

    /// # Safety
    /// This should only be called before a call to [`Self::data`].
    /// Multiple calls are allowed.
    pub unsafe fn data_ref(&self) -> Option<&T> {
        let on_heap = self.ptr.as_ptr();
        (*on_heap).data.as_ref()
    }

    pub fn into_user_data(self) -> u64 {
        let ptr = self.ptr.as_ptr() as u64;
        mem::forget(self); // will be dropped kernel-side instead
        ptr
    }

    pub fn place_clone_in_kernel(&self) -> u64 {
        let clone = self.clone();
        clone.into_user_data()
    }
}

impl<H> KernelOwned<H, ()> {
    pub fn from_user_data(user_data: u64) -> Self {
        Self {
            ptr: unsafe { ptr::NonNull::new_unchecked(user_data as *mut _) },
        }
    }
}

// copied from Arc's Clone & Drop

impl<H, T> Clone for KernelOwned<H, T> {
    fn clone(&self) -> Self {
        self.header().ref_count.fetch_add(1, Ordering::Relaxed);

        Self { ptr: self.ptr }
    }
}

impl<H, T> Drop for KernelOwned<H, T> {
    fn drop(&mut self) {
        let header = self.header();
        if header.ref_count.fetch_sub(1, Ordering::Release) == 1 {
            atomic::fence(Ordering::Acquire);
            (header.dealloc)(unsafe { self.ptr.as_mut() as *mut _ as *mut _ });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::future::CompletionState;

    #[test]
    fn kernel_owned_size_is_u64() {
        let owned = KernelOwned::new(CompletionState::Unsubmitted, Vec::<u8>::with_capacity(1024));
        assert_eq!(std::mem::size_of::<u64>(), std::mem::size_of_val(&owned));
    }

    static DROP_COUNT: AtomicUsize = AtomicUsize::new(0);

    struct Checker;

    impl Checker {
        fn new() -> Self {
            DROP_COUNT.store(0, Ordering::SeqCst);
            Self
        }
    }

    impl Drop for Checker {
        fn drop(&mut self) {
            DROP_COUNT.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn check_drop_count(expected: usize) {
        assert_eq!(DROP_COUNT.load(Ordering::SeqCst), expected);
    }

    #[test]
    fn stress_test_clone() {
        let checker = KernelOwned::new(CompletionState::Unsubmitted, Checker::new());

        let threads = (0..100)
            .map(|_| {
                let clone = checker.clone();
                let user_data = clone.into_user_data();
                let join_handle = std::thread::spawn(move || {
                    let tmp = KernelOwned::<CompletionState, ()>::from_user_data(user_data);
                    for _ in 0..1_000_000 {
                        let _ = tmp.clone();
                    }
                });
                join_handle
            })
            .collect::<Vec<_>>();
        threads.into_iter().for_each(|t| t.join().unwrap());
        drop(checker);
        check_drop_count(1);
    }
}
