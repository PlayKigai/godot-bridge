//! The single platform boundary of the bridge.
//!
//! Every `libc`, `std::os::unix` and `windows-sys` call lives under this
//! module. The rest of the crate is platform neutral and reaches the operating
//! system only through the API re-exported here, which both `unix` and
//! `windows` implement with the same names and signatures.

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::*;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::*;

/// How [`open_private`] should open a file that only the current user may read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpenMode {
    /// Open for reading and writing, creating the file, keeping its contents.
    Create,
    /// Open for writing, creating the file and discarding its contents.
    Truncate,
    /// Open for appending, creating the file.
    Append,
}
