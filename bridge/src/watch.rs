use std::collections::HashMap;
use std::ffi::CString;
use std::io;
use std::os::fd::RawFd;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use tokio::sync::mpsc::{self, Receiver, Sender};

use crate::docs_state::{directory_is_skipped, WatcherChange, WatcherChangeKind};
use crate::root::canonical_or_normalized;

const WATCH_MASK: u32 = libc::IN_CLOSE_WRITE
    | libc::IN_CREATE
    | libc::IN_DELETE
    | libc::IN_DELETE_SELF
    | libc::IN_IGNORED
    | libc::IN_MOVED_FROM
    | libc::IN_MOVED_TO
    | libc::IN_MOVE_SELF;
const POLL_TIMEOUT_MS: i32 = 100;
const EVENT_BUFFER_SIZE: usize = 64 * 1024;

pub struct ProjectWatcher {
    pub(crate) receiver: Receiver<io::Result<WatcherChange>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

struct WatcherState {
    fd: RawFd,
    project: PathBuf,
    diagnose_addons: bool,
    watch_paths: HashMap<RawFd, PathBuf>,
    watch_limit_reached: bool,
    rescan_pending: bool,
}

pub fn watch_project(project: &Path, diagnose_addons: bool) -> io::Result<ProjectWatcher> {
    let project = canonical_or_normalized(project);
    let fd = unsafe { libc::inotify_init1(libc::IN_CLOEXEC | libc::IN_NONBLOCK) };
    if fd == -1 {
        return Err(io::Error::last_os_error());
    }
    let mut state = WatcherState {
        fd,
        project: project.clone(),
        diagnose_addons,
        watch_paths: HashMap::new(),
        watch_limit_reached: false,
        rescan_pending: false,
    };
    if let Err(error) = state.watch_directory(&project) {
        unsafe {
            libc::close(fd);
        }
        return Err(error);
    }

    let (sender, receiver) = mpsc::channel(1024);
    let stop = Arc::new(AtomicBool::new(false));
    let stop_for_thread = Arc::clone(&stop);
    let thread = match thread::Builder::new()
        .name("godot-bridge-watch".to_owned())
        .spawn(move || watch_events(state, sender, stop_for_thread))
    {
        Ok(thread) => thread,
        Err(error) => {
            unsafe {
                libc::close(fd);
            }
            return Err(error);
        }
    };
    Ok(ProjectWatcher {
        receiver,
        stop,
        thread: Some(thread),
    })
}

impl Drop for ProjectWatcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl WatcherState {
    fn watch_directory(&mut self, path: &Path) -> io::Result<()> {
        if directory_is_skipped(path, &self.project, self.diagnose_addons) {
            return Ok(());
        }
        if self.add_watch(path)? {
            let entries = std::fs::read_dir(path)?;
            for entry in entries.flatten() {
                let entry_path = entry.path();
                let Ok(file_type) = entry.file_type() else {
                    continue;
                };
                if !file_type.is_dir() || file_type.is_symlink() {
                    continue;
                }
                if let Err(error) = self.watch_directory(&entry_path) {
                    crate::warn!(
                        "skipping unreadable diagnostics watch directory {}: {error}",
                        entry_path.display()
                    );
                }
            }
        }
        Ok(())
    }

    fn add_watch(&mut self, path: &Path) -> io::Result<bool> {
        if self.watch_limit_reached {
            return Ok(false);
        }
        let path_bytes = path.as_os_str().as_bytes();
        let c_path = CString::new(path_bytes)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "watch path contains NUL"))?;
        let wd = unsafe { libc::inotify_add_watch(self.fd, c_path.as_ptr(), WATCH_MASK) };
        if wd == -1 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ENOSPC) {
                self.watch_limit_reached = true;
                crate::warn!("inotify watch limit reached; project diagnostics watch is partial");
                return Ok(false);
            }
            return Err(error);
        }
        self.watch_paths.insert(wd, path.to_owned());
        Ok(true)
    }

    fn read_events(&mut self, sender: &Sender<io::Result<WatcherChange>>) -> io::Result<bool> {
        let mut buffer = [0u8; EVENT_BUFFER_SIZE];
        let size = unsafe { libc::read(self.fd, buffer.as_mut_ptr().cast(), buffer.len()) };
        if size == -1 {
            let error = io::Error::last_os_error();
            if error
                .raw_os_error()
                .is_some_and(|errno| errno == libc::EAGAIN || errno == libc::EWOULDBLOCK)
            {
                return Ok(true);
            }
            if error.raw_os_error() == Some(libc::EINTR) {
                return Ok(true);
            }
            return Err(error);
        }
        if size == 0 {
            return Ok(false);
        }
        let bytes = &buffer[..size as usize];
        let mut offset: usize = 0;
        let header_size = std::mem::size_of::<libc::inotify_event>();
        while offset
            .checked_add(header_size)
            .is_some_and(|end| end <= bytes.len())
        {
            let event = unsafe {
                ptr::read_unaligned(bytes.as_ptr().add(offset).cast::<libc::inotify_event>())
            };
            let Some(event_size) = header_size.checked_add(event.len as usize) else {
                break;
            };
            if !offset
                .checked_add(event_size)
                .is_some_and(|end| end <= bytes.len())
            {
                break;
            }
            if !self.emit_event(
                &event,
                &bytes[offset + header_size..offset + event_size],
                sender,
            ) {
                return Ok(false);
            }
            offset += event_size;
        }
        Ok(true)
    }

    fn emit_event(
        &mut self,
        event: &libc::inotify_event,
        name_bytes: &[u8],
        sender: &Sender<io::Result<WatcherChange>>,
    ) -> bool {
        if event.mask & libc::IN_Q_OVERFLOW != 0 {
            return self.send_change(
                sender,
                WatcherChange {
                    kind: WatcherChangeKind::Rescan,
                    path: self.project.clone(),
                },
            );
        }
        if event.mask & libc::IN_IGNORED != 0 {
            self.watch_paths.remove(&event.wd);
            return true;
        }
        let Some(directory) = self.watch_paths.get(&event.wd).cloned() else {
            return true;
        };
        let path = if let Some(end) = name_bytes.iter().position(|byte| *byte == 0) {
            directory.join(PathBuf::from(std::ffi::OsString::from_vec(
                name_bytes[..end].to_vec(),
            )))
        } else if !name_bytes.is_empty() {
            directory.join(PathBuf::from(std::ffi::OsString::from_vec(
                name_bytes.to_vec(),
            )))
        } else {
            directory.clone()
        };
        if event.mask & libc::IN_DELETE_SELF != 0 {
            self.watch_paths.remove(&event.wd);
            return self.send_change(
                sender,
                WatcherChange {
                    kind: WatcherChangeKind::Removed,
                    path,
                },
            );
        }
        if event.mask & libc::IN_MOVE_SELF != 0 {
            self.watch_paths.remove(&event.wd);
        }
        if event.mask & libc::IN_ISDIR != 0
            && event.mask & (libc::IN_CREATE | libc::IN_MOVED_TO) != 0
            && !directory_is_skipped(&path, &self.project, self.diagnose_addons)
        {
            if let Err(error) = self.watch_directory(&path) {
                crate::warn!(
                    "skipping unreadable diagnostics watch directory {}: {error}",
                    path.display()
                );
            }
        }
        let kind = if event.mask & (libc::IN_DELETE | libc::IN_MOVED_FROM) != 0 {
            WatcherChangeKind::Removed
        } else if event.mask & (libc::IN_CREATE | libc::IN_MOVED_TO) != 0 {
            WatcherChangeKind::Created
        } else if event.mask & libc::IN_CLOSE_WRITE != 0 {
            WatcherChangeKind::Modified
        } else {
            return true;
        };
        self.send_change(sender, WatcherChange { kind, path })
    }

    fn send_change(
        &mut self,
        sender: &Sender<io::Result<WatcherChange>>,
        change: WatcherChange,
    ) -> bool {
        match sender.try_send(Ok(change)) {
            Ok(()) => true,
            Err(tokio::sync::mpsc::error::TrySendError::Full(Ok(change))) => {
                self.rescan_pending |= change.kind == WatcherChangeKind::Rescan;
                true
            }
            Err(tokio::sync::mpsc::error::TrySendError::Full(Err(_))) => true,
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => false,
        }
    }

    fn send_pending_rescan(&mut self, sender: &Sender<io::Result<WatcherChange>>) -> bool {
        if !self.rescan_pending {
            return true;
        }
        match sender.try_send(Ok(WatcherChange {
            kind: WatcherChangeKind::Rescan,
            path: self.project.clone(),
        })) {
            Ok(()) => {
                self.rescan_pending = false;
                true
            }
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => true,
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => false,
        }
    }
}

fn watch_events(
    mut state: WatcherState,
    sender: Sender<io::Result<WatcherChange>>,
    stop: Arc<AtomicBool>,
) {
    while !stop.load(Ordering::Acquire) {
        if !state.send_pending_rescan(&sender) {
            break;
        }
        let mut pollfd = libc::pollfd {
            fd: state.fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let result = unsafe { libc::poll(&mut pollfd, 1, POLL_TIMEOUT_MS) };
        if result == -1 {
            if io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            let _ = sender.try_send(Err(io::Error::last_os_error()));
            break;
        }
        if result == 0 {
            continue;
        }
        if pollfd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
            let _ = sender.try_send(Err(io::Error::other("inotify watch closed")));
            break;
        }
        match state.read_events(&sender) {
            Ok(true) => {}
            Ok(false) => break,
            Err(error) => {
                let _ = sender.try_send(Err(error));
                break;
            }
        }
    }
    unsafe {
        libc::close(state.fd);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::Duration;
    use tempfile::tempdir;

    #[test]
    fn watches_file_lifecycle() {
        let directory = tempdir().unwrap();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let mut watcher = watch_project(directory.path(), false).unwrap();
            let path = directory.path().join("file.gd");
            fs::write(&path, "one").unwrap();
            let mut changes = Vec::new();
            while changes.len() < 2 {
                changes.push(
                    tokio::time::timeout(Duration::from_secs(2), watcher.receiver.recv())
                        .await
                        .unwrap()
                        .unwrap()
                        .unwrap(),
                );
            }
            assert!(changes.iter().any(|change| {
                change.kind == WatcherChangeKind::Created && change.path == path
            }));
            assert!(changes.iter().any(|change| {
                change.kind == WatcherChangeKind::Modified && change.path == path
            }));

            fs::write(&path, "two").unwrap();
            let modified = tokio::time::timeout(Duration::from_secs(2), watcher.receiver.recv())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert_eq!(modified.kind, WatcherChangeKind::Modified);
            assert_eq!(modified.path, path);

            let renamed = directory.path().join("renamed.gd");
            fs::rename(&path, &renamed).unwrap();
            let mut rename_changes = Vec::new();
            while rename_changes.len() < 2 {
                rename_changes.push(
                    tokio::time::timeout(Duration::from_secs(2), watcher.receiver.recv())
                        .await
                        .unwrap()
                        .unwrap()
                        .unwrap(),
                );
            }
            assert!(rename_changes.iter().any(|change| {
                change.kind == WatcherChangeKind::Removed && change.path == path
            }));
            assert!(rename_changes.iter().any(|change| {
                change.kind == WatcherChangeKind::Created && change.path == renamed
            }));

            fs::remove_file(&renamed).unwrap();
            let removed = tokio::time::timeout(Duration::from_secs(2), watcher.receiver.recv())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert_eq!(removed.kind, WatcherChangeKind::Removed);
            assert_eq!(removed.path, renamed);
        });
    }

    #[test]
    fn watches_directories_created_after_start() {
        let directory = tempdir().unwrap();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let mut watcher = watch_project(directory.path(), false).unwrap();
            let nested = directory.path().join("nested");
            fs::create_dir(&nested).unwrap();
            loop {
                let change = tokio::time::timeout(Duration::from_secs(2), watcher.receiver.recv())
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap();
                if change.kind == WatcherChangeKind::Created && change.path == nested {
                    break;
                }
            }
            let path = nested.join("file.gd");
            fs::write(&path, "one").unwrap();
            loop {
                let change = tokio::time::timeout(Duration::from_secs(2), watcher.receiver.recv())
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap();
                if change.kind == WatcherChangeKind::Created && change.path == path {
                    break;
                }
            }
        });
    }
}
