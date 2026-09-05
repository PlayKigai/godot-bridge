use std::collections::HashMap;
use std::ffi::CString;
use std::io;
use std::os::fd::RawFd;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::ptr;
use std::thread::{self, JoinHandle};

use std::sync::mpsc::SyncSender;

use crate::docs_state::{directory_is_skipped, WatcherChange, WatcherChangeKind};
use crate::lsp::ProxyEvent;
use crate::root::canonical_or_normalized;

const WATCH_MASK: u32 = libc::IN_CLOSE_WRITE
    | libc::IN_CREATE
    | libc::IN_DELETE
    | libc::IN_DELETE_SELF
    | libc::IN_IGNORED
    | libc::IN_MOVED_FROM
    | libc::IN_MOVED_TO
    | libc::IN_MOVE_SELF
    | libc::IN_DONT_FOLLOW;
const EVENT_BUFFER_SIZE: usize = 64 * 1024;

pub struct ProjectWatcher {
    stop_write: RawFd,
    thread: Option<JoinHandle<()>>,
}

struct WatcherState {
    fd: RawFd,
    project: PathBuf,
    diagnose_addons: bool,
    watch_paths: HashMap<RawFd, PathBuf>,
    watch_limit_reached: bool,
}

pub(crate) fn watch_project_into(
    project: &Path,
    diagnose_addons: bool,
    sender: SyncSender<ProxyEvent>,
) -> io::Result<ProjectWatcher> {
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
    };
    if let Err(error) = state.watch_directory(&project) {
        unsafe {
            libc::close(fd);
        }
        return Err(error);
    }

    let mut stop_pipe = [0; 2];
    if unsafe { libc::pipe2(stop_pipe.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } == -1 {
        let error = io::Error::last_os_error();
        unsafe {
            libc::close(fd);
        }
        return Err(error);
    }
    let stop_write = stop_pipe[1];
    let thread = match thread::Builder::new()
        .name("godot-bridge-watch".to_owned())
        .stack_size(256 * 1024)
        .spawn(move || watch_events(state, stop_pipe[0], sender))
    {
        Ok(thread) => thread,
        Err(error) => {
            unsafe {
                libc::close(fd);
                libc::close(stop_pipe[0]);
                libc::close(stop_write);
            }
            return Err(error);
        }
    };
    Ok(ProjectWatcher {
        stop_write,
        thread: Some(thread),
    })
}

impl Drop for ProjectWatcher {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.stop_write);
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl WatcherState {
    fn watch_directory(&mut self, path: &Path) -> io::Result<()> {
        let root = path.to_owned();
        let mut directories = vec![root.clone()];
        while let Some(directory) = directories.pop() {
            if directory_is_skipped(&directory, &self.project, self.diagnose_addons) {
                continue;
            }
            if !self.add_watch(&directory)? {
                continue;
            }
            let entries = match std::fs::read_dir(&directory) {
                Ok(entries) => entries,
                Err(error) if directory != root => {
                    crate::warn!(
                        "skipping unreadable diagnostics watch directory {}: {error}",
                        directory.display()
                    );
                    continue;
                }
                Err(error) => return Err(error),
            };
            for entry in entries.flatten() {
                let entry_path = entry.path();
                let Ok(file_type) = entry.file_type() else {
                    continue;
                };
                if file_type.is_dir() && !file_type.is_symlink() {
                    directories.push(entry_path);
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

    fn read_events(&mut self, sender: &SyncSender<ProxyEvent>) -> io::Result<bool> {
        let mut buffer = [0u8; EVENT_BUFFER_SIZE];
        let size = unsafe { libc::read(self.fd, buffer.as_mut_ptr().cast(), buffer.len()) };
        if size == -1 {
            let error = io::Error::last_os_error();
            if error
                .raw_os_error()
                .is_some_and(|errno| matches!(errno, libc::EAGAIN | libc::EINTR))
            {
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
        sender: &SyncSender<ProxyEvent>,
    ) -> bool {
        if event.mask & libc::IN_Q_OVERFLOW != 0 {
            return sender
                .send(ProxyEvent::Watcher(Ok(WatcherChange {
                    kind: WatcherChangeKind::Rescan,
                    path: self.project.clone(),
                })))
                .is_ok();
        }
        if event.mask & libc::IN_IGNORED != 0 {
            self.watch_paths.remove(&event.wd);
            return true;
        }
        let Some(directory) = self.watch_paths.get(&event.wd).cloned() else {
            return true;
        };
        let end = name_bytes
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(name_bytes.len());
        let path = if end == 0 {
            directory.clone()
        } else {
            directory.join(PathBuf::from(std::ffi::OsString::from_vec(
                name_bytes[..end].to_vec(),
            )))
        };
        if event.mask & libc::IN_DELETE_SELF != 0 {
            self.watch_paths.remove(&event.wd);
            return sender
                .send(ProxyEvent::Watcher(Ok(WatcherChange {
                    kind: WatcherChangeKind::Removed,
                    path,
                })))
                .is_ok();
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
        sender
            .send(ProxyEvent::Watcher(Ok(WatcherChange { kind, path })))
            .is_ok()
    }
}

fn watch_events(mut state: WatcherState, stop_read: RawFd, sender: SyncSender<ProxyEvent>) {
    loop {
        let mut pollfds = [
            libc::pollfd {
                fd: state.fd,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: stop_read,
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let result = unsafe { libc::poll(pollfds.as_mut_ptr(), 2, -1) };
        if result == -1 {
            if io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            let _ = sender.send(ProxyEvent::Watcher(Err(io::Error::last_os_error())));
            break;
        }
        if pollfds[1].revents != 0 {
            break;
        }
        if pollfds[0].revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
            let _ = sender.send(ProxyEvent::Watcher(Err(io::Error::other(
                "inotify watch closed",
            ))));
            break;
        }
        match state.read_events(&sender) {
            Ok(true) => {}
            Ok(false) => break,
            Err(error) => {
                let _ = sender.send(ProxyEvent::Watcher(Err(error)));
                break;
            }
        }
    }
    unsafe {
        libc::close(state.fd);
        libc::close(stop_read);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::temp::TempDir;
    use std::fs;
    use std::sync::mpsc;
    use std::time::Duration;

    fn receive_change(receiver: &mpsc::Receiver<ProxyEvent>) -> WatcherChange {
        let Ok(ProxyEvent::Watcher(change)) = receiver.recv_timeout(Duration::from_secs(2)) else {
            panic!();
        };
        change.unwrap()
    }

    #[test]
    fn watches_file_lifecycle() {
        let directory = TempDir::new().unwrap();
        let (sender, receiver) = mpsc::sync_channel(4096);
        let _watcher = watch_project_into(directory.path(), false, sender).unwrap();
        let path = directory.path().join("file.gd");
        fs::write(&path, "one").unwrap();
        let mut changes = Vec::new();
        while changes.len() < 2 {
            changes.push(receive_change(&receiver));
        }
        assert!(changes
            .iter()
            .any(|change| { change.kind == WatcherChangeKind::Created && change.path == path }));
        assert!(changes
            .iter()
            .any(|change| { change.kind == WatcherChangeKind::Modified && change.path == path }));

        fs::write(&path, "two").unwrap();
        let modified = receive_change(&receiver);
        assert_eq!(modified.kind, WatcherChangeKind::Modified);
        assert_eq!(modified.path, path);

        let renamed = directory.path().join("renamed.gd");
        fs::rename(&path, &renamed).unwrap();
        let mut rename_changes = Vec::new();
        while rename_changes.len() < 2 {
            rename_changes.push(receive_change(&receiver));
        }
        assert!(rename_changes
            .iter()
            .any(|change| { change.kind == WatcherChangeKind::Removed && change.path == path }));
        assert!(rename_changes
            .iter()
            .any(|change| { change.kind == WatcherChangeKind::Created && change.path == renamed }));

        fs::remove_file(&renamed).unwrap();
        let removed = receive_change(&receiver);
        assert_eq!(removed.kind, WatcherChangeKind::Removed);
        assert_eq!(removed.path, renamed);
    }

    #[test]
    fn watches_directories_created_after_start() {
        let directory = TempDir::new().unwrap();
        let (sender, receiver) = mpsc::sync_channel(4096);
        let _watcher = watch_project_into(directory.path(), false, sender).unwrap();
        let nested = directory.path().join("nested");
        fs::create_dir(&nested).unwrap();
        loop {
            let change = receive_change(&receiver);
            if change.kind == WatcherChangeKind::Created && change.path == nested {
                break;
            }
        }
        let path = nested.join("file.gd");
        fs::write(&path, "one").unwrap();
        loop {
            let change = receive_change(&receiver);
            if change.kind == WatcherChangeKind::Created && change.path == path {
                break;
            }
        }
    }
}
