#[cfg(test)]
mod tests {
    use std::{
        borrow::Borrow,
        error::Error,
        fs::File,
        os::fd::{AsFd, AsRawFd},
    };

    use io_uring::{opcode, types::Fd, IoUring};

    use super::*;

    #[test]
    fn basic_read() -> Result<(), Box<dyn Error>> {
        let mut ring = IoUring::new(100)?;
        let mut buf = [0; 1024];
        let file = File::open("Cargo.toml")?;
        let read = opcode::Read::new(Fd(file.as_raw_fd()), (&mut buf).as_mut_ptr(), 1024);
        let read_entry = read.build();
        unsafe { ring.submission().push(&read_entry)? }
        ring.submit_and_wait(1)?;
        let entry = ring.completion().next().expect("one read entry");
        let result = entry.result();
        if result < 0 {
            return Err(std::io::Error::from_raw_os_error(-result as i32).into());
        }
        let buf = String::from_utf8_lossy(&buf[..result as usize]);
        assert!(buf.contains("vilya"), "io_uring read failed");
        Ok(())
    }
}
