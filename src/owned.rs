use std::sync::atomic::{self, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::{mem, ptr};

use crate::future::CompletionState;

#[repr(C)]
struct KernelOwnedOnHeapHeader {
    ref_count: AtomicUsize,
    dealloc: fn(*mut Self),
    state: Mutex<CompletionState>,
}

#[repr(C)]
struct KernelOwnedOnHeap<T> {
    header: KernelOwnedOnHeapHeader,
    data: Option<T>,
}

impl<T: Send + 'static> KernelOwnedOnHeap<T> {
    pub fn new(data: T) -> Self {
        Self {
            header: KernelOwnedOnHeapHeader {
                ref_count: AtomicUsize::new(1),
                dealloc: |this| {
                    drop(unsafe { Box::<Self>::from_raw(this as *mut _) });
                },
                state: Mutex::new(CompletionState::Unsubmitted),
            },
            data: Some(data),
        }
    }
}

pub struct KernelOwned<T> {
    ptr: ptr::NonNull<KernelOwnedOnHeap<T>>,
}

unsafe impl<T> Send for KernelOwned<T> {}

impl<T: Send + 'static> KernelOwned<T> {
    pub fn new(data: T) -> Self {
        let ptr = Box::into_raw(Box::new(KernelOwnedOnHeap::new(data)));

        Self {
            ptr: unsafe { ptr::NonNull::new_unchecked(ptr) },
        }
    }
}

impl<T> KernelOwned<T> {
    fn header(&self) -> &KernelOwnedOnHeapHeader {
        unsafe { &self.ptr.as_ref().header }
    }

    pub fn state(&self) -> &Mutex<CompletionState> {
        &self.header().state
    }

    pub fn data(mut self) -> Option<T> {
        let on_heap = unsafe { self.ptr.as_mut() };
        if on_heap.header.ref_count.load(Ordering::Relaxed) == 1 {
            atomic::fence(Ordering::Acquire);
            let data = mem::replace(&mut on_heap.data, None);
            (on_heap.header.dealloc)(unsafe { self.ptr.as_mut() as *mut _ as *mut _ });
            mem::forget(self); // don't call the normal drop
            Some(data.unwrap())
        } else {
            None
        }
    }

    pub fn to_user_data(self) -> u64 {
        let ptr = self.ptr.as_ptr() as u64;
        mem::forget(self); // will be dropped kernel-side instead
        ptr
    }

    pub fn place_in_kernel(&self) -> u64 {
        let clone = self.clone();
        clone.to_user_data()
    }
}

impl KernelOwned<()> {
    pub fn from_user_data(user_data: u64) -> Self {
        Self {
            ptr: unsafe { ptr::NonNull::new_unchecked(user_data as *mut _) },
        }
    }
}

// copied from Arc's Clone & Drop

impl<T> Clone for KernelOwned<T> {
    fn clone(&self) -> Self {
        self.header().ref_count.fetch_add(1, Ordering::Relaxed);

        Self { ptr: self.ptr }
    }
}

impl<T> Drop for KernelOwned<T> {
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

    #[test]
    fn kernel_owned_size_is_u64() {
        let owned = KernelOwned::new(Vec::<u8>::with_capacity(1024));
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
        let checker = KernelOwned::new(Checker::new());

        let threads = (0..100)
            .map(|_| {
                let clone = checker.clone();
                let user_data = clone.to_user_data();
                let join_handle = std::thread::spawn(move || {
                    let tmp = KernelOwned::from_user_data(user_data);
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
