use crate::sys;
use std::ffi;
use std::io;

pub struct Mmap {
    addr: *mut ffi::c_void,
    len: usize,
}

impl Mmap {
    // pub fn new_anon(len: usize) -> io::Result<Mmap> {
    //     let addr = unsafe {
    //         sys::mmap(
    //             std::ptr::null_mut(),
    //             len,
    //             (sys::PROT_READ | sys::PROT_WRITE) as i32,
    //             (sys::MAP_PRIVATE | sys::MAP_ANONYMOUS) as i32,
    //             -1,
    //             0,
    //         )
    //     }?;

    //     Ok(Mmap { addr, len })
    // }

    pub fn map_file(fd: sys::RawFd, len: usize, offset: sys::off_t) -> io::Result<Mmap> {
        let addr = unsafe {
            sys::mmap(
                std::ptr::null_mut(),
                len,
                (sys::PROT_READ | sys::PROT_WRITE) as i32,
                (sys::MAP_SHARED | sys::MAP_POPULATE) as i32,
                fd,
                offset,
            )
        }?;

        Ok(Mmap { addr, len })
    }
}

impl Drop for Mmap {
    fn drop(&mut self) {
        unsafe {
            sys::munmap(self.addr, self.len).unwrap();
        }
    }
}
