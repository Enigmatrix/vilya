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
