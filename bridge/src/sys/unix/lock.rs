//! Advisory whole-file locks that a dying process always releases.

use std::io;
use std::os::fd::AsRawFd;
use std::path::Path;

use crate::sys::{open_private, OpenMode};

pub struct LockGuard {
    file: std::fs::File,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
    }
}

/// Take the exclusive lock on `path` without blocking. `Ok(None)` means
/// another process holds it.
pub fn try_lock(path: &Path) -> io::Result<Option<LockGuard>> {
    let file = open_private(path, OpenMode::Create)?;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return Ok(Some(LockGuard { file }));
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::EWOULDBLOCK) {
        Ok(None)
    } else {
        Err(error)
    }
}
