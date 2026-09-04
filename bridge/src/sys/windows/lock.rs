//! Whole-file locks that the kernel releases when the holder dies.

use std::io;
use std::os::windows::io::AsRawHandle;
use std::path::Path;

use windows_sys::Win32::Foundation::ERROR_LOCK_VIOLATION;
use windows_sys::Win32::Storage::FileSystem::{
    LockFileEx, UnlockFileEx, LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY,
};
use windows_sys::Win32::System::IO::OVERLAPPED;

use crate::sys::{open_private, OpenMode};

/// The whole file is locked as one range, so the range is the largest one
/// `LockFileEx` accepts.
const RANGE_LOW: u32 = u32::MAX;
const RANGE_HIGH: u32 = u32::MAX;

pub struct LockGuard {
    file: std::fs::File,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
        unsafe {
            UnlockFileEx(
                self.file.as_raw_handle(),
                0,
                RANGE_LOW,
                RANGE_HIGH,
                &mut overlapped,
            );
        }
    }
}

/// Take the exclusive lock on `path` without blocking. `Ok(None)` means
/// another process holds it.
pub fn try_lock(path: &Path) -> io::Result<Option<LockGuard>> {
    let file = open_private(path, OpenMode::Create)?;
    let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
    let locked = unsafe {
        LockFileEx(
            file.as_raw_handle(),
            LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
            0,
            RANGE_LOW,
            RANGE_HIGH,
            &mut overlapped,
        )
    };
    if locked != 0 {
        return Ok(Some(LockGuard { file }));
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(ERROR_LOCK_VIOLATION as i32) {
        Ok(None)
    } else {
        Err(error)
    }
}
