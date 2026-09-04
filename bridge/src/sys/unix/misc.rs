use std::io;
use std::mem::ManuallyDrop;

/// Cap glibc arenas so the many short-lived bridge threads do not each claim a
/// 64 MiB heap arena. A no-op where glibc's `mallopt` does not exist.
pub fn tune_allocator() {
    #[cfg(target_os = "linux")]
    unsafe {
        libc::mallopt(libc::M_ARENA_MAX, 1);
    }
}

/// The process standard output as a plain file, so LSP frames bypass the
/// locking and line buffering of `std::io::Stdout`. The caller must not close
/// it, hence [`ManuallyDrop`].
pub fn stdout_file() -> ManuallyDrop<std::fs::File> {
    use std::os::fd::FromRawFd;
    ManuallyDrop::new(unsafe { std::fs::File::from_raw_fd(1) })
}

/// Hand a URL to the desktop's browser. The caller has already validated it;
/// `xdg-open` receives it as one argument, never through a shell.
pub fn open_url(url: &str) -> io::Result<()> {
    let status = std::process::Command::new("xdg-open").arg(url).status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!("xdg-open exited {status}")))
    }
}
