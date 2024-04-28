#[allow(dead_code)]
#[allow(non_upper_case_globals)]
#[allow(non_camel_case_types)]
#[allow(non_snake_case)]
#[allow(clippy::all, clippy::pedantic, clippy::restriction, clippy::nursery)]
mod bindings {
    #[cfg(all(feature = "bindgen", not(feature = "overwrite")))]
    include!(concat!(env!("OUT_DIR"), "/sys.rs"));

    #[cfg(any(
        not(feature = "bindgen"),
        all(feature = "bindgen", feature = "overwrite")
    ))]
    include!("sys.rs");
}

pub use bindings::*;

use core::ffi;
use std::io;

fn resultify(ret: ffi::c_int) -> io::Result<ffi::c_int> {
    if ret >= 0 {
        Ok(ret)
    } else {
        Err(io::Error::from_raw_os_error(-ret))
    }
}

pub unsafe fn io_uring_register(
    fd: ffi::c_int,
    opcode: ffi::c_uint,
    arg: *const ffi::c_void,
    nr_args: ffi::c_uint,
) -> io::Result<ffi::c_int> {
    resultify(sc::syscall4(
        __NR_io_uring_register as usize,
        fd as usize,
        opcode as usize,
        arg as usize,
        nr_args as usize,
    ) as _)
}

pub unsafe fn io_uring_setup(
    entries: ffi::c_uint,
    p: *mut io_uring_params,
) -> io::Result<ffi::c_int> {
    resultify(sc::syscall2(__NR_io_uring_setup as usize, entries as usize, p as usize) as _)
}

pub unsafe fn io_uring_enter(
    fd: ffi::c_int,
    to_submit: ffi::c_uint,
    min_complete: ffi::c_uint,
    flags: ffi::c_uint,
    arg: *const ffi::c_void,
    size: usize,
) -> io::Result<ffi::c_int> {
    resultify(sc::syscall6(
        __NR_io_uring_enter as usize,
        fd as usize,
        to_submit as usize,
        min_complete as usize,
        flags as usize,
        arg as usize,
        size,
    ) as _)
}
