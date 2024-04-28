// referencing https://github.com/tokio-rs/io-uring/blob/501ee78049fa785eb4f5888252a0983ba6cf78e0/build.rs

#[cfg(not(feature = "bindgen"))]
fn main() {}

#[cfg(feature = "bindgen")]
fn main() {
    use std::env;
    use std::path::PathBuf;

    const INCLUDE: &str = r#"
#include <unistd.h>
#include <sys/syscall.h>
#include <linux/mman.h>
#include <linux/time_types.h>
#include <linux/stat.h>
#include <linux/openat2.h>
#include <linux/io_uring.h>
#include <linux/futex.h>
    "#;

    #[cfg(not(feature = "overwrite"))]
    let outdir = PathBuf::from(env::var("OUT_DIR").unwrap());

    #[cfg(feature = "overwrite")]
    let outdir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap()).join("src/sys");

    let mut builder = bindgen::Builder::default();

    if let Some(path) = env::var("BUILD_IO_URING_INCLUDE_FILE")
        .ok()
        .filter(|path| !path.is_empty())
    {
        builder = builder.header(path);
    } else {
        builder = builder.header_contents("include-file.h", INCLUDE);
    }

    builder
        .ctypes_prefix("::core::ffi") // CHANGED: originally was libc
        .prepend_enum_name(false)
        .derive_default(true)
        .generate_comments(true)
        .use_core()
        .allowlist_type("io_uring_.*|io_.qring_.*|__kernel_timespec|open_how|futex_waitv")
        .allowlist_var("__NR_io_uring.*|IOSQE_.*|IORING_.*|IO_URING_.*|SPLICE_F_FD_IN_FIXED|MAP_.*|PROT_.*|__NR_mmap|__NR_munmap")
        .generate()
        .unwrap()
        .write_to_file(outdir.join("sys.rs"))
        .unwrap();
}
