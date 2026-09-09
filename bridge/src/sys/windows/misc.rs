use std::ffi::OsStr;
use std::io;
use std::mem::ManuallyDrop;
use std::os::windows::ffi::OsStrExt;

use windows_sys::Win32::UI::Shell::ShellExecuteW;
use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

/// `ShellExecuteW` reports failure as a value at or below 32, the last of the
/// error codes it borrows from the days of `WinExec`.
const SHELL_EXECUTE_MIN_SUCCESS: isize = 32;

/// Nothing to tune: the Windows heap has no per-thread arenas to cap.
pub fn tune_allocator() {}

/// The process standard output as a plain file, so LSP frames bypass the
/// locking and line buffering of `std::io::Stdout`. The caller must not close
/// it, hence [`ManuallyDrop`].
pub fn stdout_file() -> ManuallyDrop<std::fs::File> {
    use std::os::windows::io::{AsRawHandle, FromRawHandle};
    ManuallyDrop::new(unsafe { std::fs::File::from_raw_handle(std::io::stdout().as_raw_handle()) })
}

/// Hand a URL to the default browser. The caller has already validated it;
/// the shell association is invoked directly, never through a command line
/// that would reinterpret the characters in it.
pub fn open_url(url: &str) -> io::Result<()> {
    let operation = wide("open");
    let file = wide(url);
    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            operation.as_ptr(),
            file.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    if result as isize > SHELL_EXECUTE_MIN_SUCCESS {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn wide(value: &str) -> Vec<u16> {
    OsStr::new(value).encode_wide().chain(Some(0)).collect()
}
