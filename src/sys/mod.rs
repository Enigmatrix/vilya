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

use std::io;
pub use std::os::fd::RawFd;

pub use core::ffi::*;
pub type off_t = isize;

fn resultify(ret: c_int) -> io::Result<c_int> {
    if ret >= 0 {
        Ok(ret)
    } else {
        Err(io::Error::from_raw_os_error(-ret))
    }
}

pub unsafe fn io_uring_register(
    fd: c_int,
    opcode: c_uint,
    arg: *const c_void,
    nr_args: c_uint,
) -> io::Result<c_int> {
    resultify(sc::syscall4(
        __NR_io_uring_register as usize,
        fd as usize,
        opcode as usize,
        arg as usize,
        nr_args as usize,
    ) as _)
}

pub unsafe fn io_uring_setup(entries: c_uint, p: *mut io_uring_params) -> io::Result<RawFd> {
    resultify(sc::syscall2(__NR_io_uring_setup as usize, entries as usize, p as usize) as _)
}

pub unsafe fn io_uring_enter(
    fd: c_int,
    to_submit: c_uint,
    min_complete: c_uint,
    flags: c_uint,
    arg: *const c_void,
    size: usize,
) -> io::Result<c_int> {
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

pub unsafe fn mmap(
    addr: *mut c_void,
    len: usize,
    prot: c_int,
    flags: c_int,
    fd: c_int,
    offset: off_t,
) -> io::Result<*mut c_void> {
    match sc::syscall6(
        __NR_mmap as usize,
        addr as usize,
        len as usize,
        prot as usize,
        flags as usize,
        fd as usize,
        offset as usize,
    ) {
        // MAP_FAILED doesn't seem to be defined in the headers
        usize::MAX => Err(io::Error::last_os_error()),
        addr => Ok(addr as *mut c_void),
    }
}

pub unsafe fn munmap(addr: *mut c_void, len: usize) -> io::Result<()> {
    resultify(sc::syscall2(__NR_mmap as usize, addr as usize, len as usize) as _)?;
    Ok(())
}
