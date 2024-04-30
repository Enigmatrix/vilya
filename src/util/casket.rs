use std::{
    mem::{self, ManuallyDrop},
    ops::{Deref, DerefMut},
    ptr, sync::Arc,
};

/// C++ class-style vtable-based wrapper to mimic `dyn Drop`
#[repr(C)]
pub struct Casket<T> {
    drop: Option<fn(&mut T)>,
    data: ManuallyDrop<T>,
}

impl<T> Casket<T> {
    /// Create a new `Casket` with the given data
    pub fn new(data: T) -> Self {
        let drop: Option<fn(&mut T)> = {
            if mem::needs_drop::<T>() {
                Some(|x| unsafe { ptr::drop_in_place(x) })
            } else {
                None
            }
        };
        Self {
            drop,
            data: ManuallyDrop::new(data),
        }
    }
}

impl<T> Deref for Casket<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &*self.data
    }
}

impl<T> DerefMut for Casket<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut *self.data
    }
}

impl<T> Drop for Casket<T> {
    fn drop(&mut self) {
        if let Some(drop) = self.drop {
            drop(&mut *self.data);
        }
    }
}
