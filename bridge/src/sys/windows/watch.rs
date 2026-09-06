//! `ReadDirectoryChangesW` watch of the project tree, one subtree watch on the
//! project root and one thread.

use std::collections::HashSet;
use std::ffi::OsString;
use std::io;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::mpsc::{SyncSender, TrySendError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_NOTIFY_ENUM_DIR, HANDLE, INVALID_HANDLE_VALUE, WAIT_EVENT, WAIT_OBJECT_0,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, ReadDirectoryChangesW, FILE_ACTION_ADDED, FILE_ACTION_MODIFIED,
    FILE_ACTION_REMOVED, FILE_ACTION_RENAMED_NEW_NAME, FILE_ACTION_RENAMED_OLD_NAME,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OVERLAPPED, FILE_LIST_DIRECTORY,
    FILE_NOTIFY_CHANGE_DIR_NAME, FILE_NOTIFY_CHANGE_FILE_NAME, FILE_NOTIFY_CHANGE_LAST_WRITE,
    FILE_NOTIFY_CHANGE_SIZE, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::Threading::{
    CreateEventW, SetEvent, WaitForMultipleObjects, WaitForSingleObject, INFINITE,
};
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};

use crate::docs_state::{directory_is_skipped, WatcherChange, WatcherChangeKind};
use crate::lsp::ProxyEvent;
use crate::root::canonical_or_normalized;

const NOTIFY_FILTER: u32 = FILE_NOTIFY_CHANGE_FILE_NAME
    | FILE_NOTIFY_CHANGE_DIR_NAME
    | FILE_NOTIFY_CHANGE_LAST_WRITE
    | FILE_NOTIFY_CHANGE_SIZE;
/// The buffer is held as `u32` words because `ReadDirectoryChangesW` needs a
/// DWORD aligned buffer, which `Vec<u8>` does not promise.
const BUFFER_WORDS: usize = 64 * 1024 / 4;
/// `NextEntryOffset`, `Action` and `FileNameLength` of `FILE_NOTIFY_INFORMATION`.
const RECORD_HEADER: usize = 12;
const TRUE: i32 = 1;
const FALSE: i32 = 0;
const STOP_SIGNALLED: WAIT_EVENT = WAIT_OBJECT_0 + 1;
/// How long one attempt at a full channel waits before the stop event is
/// checked again.
const PUBLISH_POLL: Duration = Duration::from_millis(100);

/// Sets the stop event and joins the watch thread when dropped.
pub(crate) struct ProjectWatcher {
    stop: OwnedHandle,
    thread: Option<JoinHandle<()>>,
}

/// Closes the handle when dropped.
struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

/// The kernel objects and the heap buffer one `ReadDirectoryChangesW` call
/// needs. The buffer and the `OVERLAPPED` are boxed so their addresses survive
/// the move onto the watch thread, and dropping this cancels a read still in
/// flight before either is freed.
struct Watch {
    directory: OwnedHandle,
    completion: OwnedHandle,
    /// Owned by the [`ProjectWatcher`], which closes it only after the watch
    /// thread has been joined.
    stop: HANDLE,
    overlapped: Box<OVERLAPPED>,
    buffer: Box<[u32]>,
}

/// A kernel handle names the same object in every thread of the process.
unsafe impl Send for Watch {}

/// Turns the names `ReadDirectoryChangesW` reports into project paths.
struct Mapper {
    project: PathBuf,
    diagnose_addons: bool,
    known_directories: HashSet<PathBuf>,
}

/// Watch every directory of the project tree and stream changes to the proxy.
/// The watch stops when the returned handle is dropped.
pub(crate) fn watch_project_into(
    project: &Path,
    diagnose_addons: bool,
    sender: SyncSender<ProxyEvent>,
) -> io::Result<ProjectWatcher> {
    let project = canonical_or_normalized(project);
    let stop = create_event()?;
    let completion = create_event()?;
    let mut overlapped: Box<OVERLAPPED> = Box::new(unsafe { std::mem::zeroed() });
    overlapped.hEvent = completion.0;
    let mut watch = Watch {
        directory: open_directory(&project)?,
        completion,
        stop: stop.0,
        overlapped,
        buffer: vec![0u32; BUFFER_WORDS].into_boxed_slice(),
    };
    // The kernel reports nothing that happened before the first read, so the
    // watch is armed before this function returns.
    watch.start_read()?;
    let mut mapper = Mapper {
        project,
        diagnose_addons,
        known_directories: HashSet::new(),
    };
    let thread = thread::Builder::new()
        .name("godot-bridge-watch".to_owned())
        .stack_size(256 * 1024)
        .spawn(move || run_watch(&mut watch, &mut mapper, &sender))?;
    Ok(ProjectWatcher {
        stop,
        thread: Some(thread),
    })
}

impl Drop for ProjectWatcher {
    fn drop(&mut self) {
        unsafe { SetEvent(self.stop.0) };
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn open_directory(project: &Path) -> io::Result<OwnedHandle> {
    let wide = wide_path(project)?;
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            FILE_LIST_DIRECTORY,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED,
            ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    Ok(OwnedHandle(handle))
}

fn wide_path(path: &Path) -> io::Result<Vec<u16>> {
    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    if wide.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "watch path contains NUL",
        ));
    }
    wide.push(0);
    Ok(wide)
}

fn create_event() -> io::Result<OwnedHandle> {
    let handle = unsafe { CreateEventW(ptr::null(), TRUE, FALSE, ptr::null()) };
    if handle.is_null() {
        return Err(io::Error::last_os_error());
    }
    Ok(OwnedHandle(handle))
}

/// The kernel drops the notification queue when it overflows and asks the
/// caller to enumerate the directory itself, which the proxy does on a rescan.
fn is_overflow(error: &io::Error) -> bool {
    error.raw_os_error() == Some(ERROR_NOTIFY_ENUM_DIR as i32)
}

impl Watch {
    fn start_read(&mut self) -> io::Result<()> {
        let length = (self.buffer.len() * 4) as u32;
        let started = unsafe {
            ReadDirectoryChangesW(
                self.directory.0,
                self.buffer.as_mut_ptr().cast(),
                length,
                TRUE,
                NOTIFY_FILTER,
                ptr::null_mut(),
                &mut *self.overlapped,
                None,
            )
        };
        if started == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn wait_for_read_or_stop(&self) -> WAIT_EVENT {
        let handles = [self.completion.0, self.stop];
        unsafe { WaitForMultipleObjects(2, handles.as_ptr(), FALSE, INFINITE) }
    }

    fn completed_bytes(&self) -> io::Result<usize> {
        let mut transferred = 0u32;
        let ok = unsafe {
            GetOverlappedResult(self.directory.0, &*self.overlapped, &mut transferred, TRUE)
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(transferred as usize)
    }

    fn records(&self, transferred: usize) -> &[u8] {
        let bytes = unsafe {
            std::slice::from_raw_parts(self.buffer.as_ptr().cast::<u8>(), self.buffer.len() * 4)
        };
        &bytes[..transferred.min(bytes.len())]
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        unsafe { CancelIoEx(self.directory.0, &*self.overlapped) };
        let mut transferred = 0u32;
        unsafe { GetOverlappedResult(self.directory.0, &*self.overlapped, &mut transferred, TRUE) };
    }
}

/// Publishes to the proxy without ever blocking past the stop event, so a full
/// channel cannot hold up [`ProjectWatcher::drop`].
struct Publisher<'a> {
    sender: &'a SyncSender<ProxyEvent>,
    stop: HANDLE,
}

impl Publisher<'_> {
    /// `false` means the watch must end: the receiver is gone, or the stop
    /// event fired while the channel stayed full.
    fn send(&self, event: ProxyEvent) -> bool {
        let mut pending = event;
        loop {
            match self.sender.try_send(pending) {
                Ok(()) => return true,
                Err(TrySendError::Full(returned)) => {
                    if self.stopped() {
                        return false;
                    }
                    pending = returned;
                }
                Err(TrySendError::Disconnected(_)) => return false,
            }
        }
    }

    /// Waits out one [`PUBLISH_POLL`] before the next attempt, and returns as
    /// soon as the stop event fires within it.
    fn stopped(&self) -> bool {
        let millis = u32::try_from(PUBLISH_POLL.as_millis()).unwrap_or(u32::MAX);
        let waited = unsafe { WaitForSingleObject(self.stop, millis) };
        waited == WAIT_OBJECT_0
    }
}

fn run_watch(watch: &mut Watch, mapper: &mut Mapper, sender: &SyncSender<ProxyEvent>) {
    let publisher = Publisher {
        sender,
        stop: watch.stop,
    };
    loop {
        let wait = watch.wait_for_read_or_stop();
        if wait != WAIT_OBJECT_0 {
            if wait != STOP_SIGNALLED {
                publisher.send(ProxyEvent::Watcher(Err(io::Error::last_os_error())));
            }
            return;
        }
        let transferred = match watch.completed_bytes() {
            Ok(transferred) => transferred,
            Err(error) if is_overflow(&error) => 0,
            Err(error) => {
                publisher.send(ProxyEvent::Watcher(Err(error)));
                return;
            }
        };
        // The buffer is decoded and the read re-armed before anything is
        // published, so a full channel backs up in the kernel queue, whose own
        // overflow already arrives as a rescan.
        let records = parse_records(watch.records(transferred));
        let mut rescan = transferred == 0;
        let armed = restart_read(watch, &mut rescan);
        for record in &records {
            if !mapper.emit_record(record, &publisher) {
                return;
            }
        }
        if rescan && !mapper.send_rescan(&publisher) {
            return;
        }
        if let Err(error) = armed {
            publisher.send(ProxyEvent::Watcher(Err(error)));
            return;
        }
    }
}

/// A read that reports the overflow of the one before it is retried once, with
/// a rescan standing in for the notifications the kernel dropped.
fn restart_read(watch: &mut Watch, rescan: &mut bool) -> io::Result<()> {
    for _ in 0..2 {
        match watch.start_read() {
            Ok(()) => return Ok(()),
            Err(error) if is_overflow(&error) => *rescan = true,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::last_os_error())
}

fn parse_records(bytes: &[u8]) -> Vec<Record> {
    let mut records = Vec::new();
    let mut offset = 0usize;
    loop {
        let Some(record) = Record::parse(&bytes[offset..]) else {
            return records;
        };
        let next_offset = record.next_offset;
        records.push(record);
        let Some(next) = offset.checked_add(next_offset) else {
            return records;
        };
        if next_offset == 0 || next >= bytes.len() {
            return records;
        }
        offset = next;
    }
}

impl Mapper {
    fn send_rescan(&self, publisher: &Publisher<'_>) -> bool {
        publisher.send(ProxyEvent::Watcher(Ok(WatcherChange {
            kind: WatcherChangeKind::Rescan,
            path: self.project.clone(),
        })))
    }

    fn emit_record(&mut self, record: &Record, publisher: &Publisher<'_>) -> bool {
        let path = self.project.join(&record.name);
        let Some(parent) = path.parent() else {
            return true;
        };
        if directory_is_skipped(parent, &self.project, self.diagnose_addons) {
            return true;
        }
        if self.is_directory(&path, record.action) {
            if record.action == FILE_ACTION_MODIFIED {
                return true;
            }
            return self.send_rescan(publisher);
        }
        let kind = match record.action {
            FILE_ACTION_ADDED | FILE_ACTION_RENAMED_NEW_NAME => WatcherChangeKind::Created,
            FILE_ACTION_REMOVED | FILE_ACTION_RENAMED_OLD_NAME => WatcherChangeKind::Removed,
            FILE_ACTION_MODIFIED => WatcherChangeKind::Modified,
            _ => return true,
        };
        publisher.send(ProxyEvent::Watcher(Ok(WatcherChange { kind, path })))
    }

    /// A vanished entry cannot be inspected, so a name seen as a directory
    /// earlier in this watch, or an extensionless removed name, counts as one.
    fn is_directory(&mut self, path: &Path, action: u32) -> bool {
        match std::fs::metadata(path) {
            Ok(metadata) if metadata.is_dir() => {
                self.known_directories.insert(path.to_owned());
                true
            }
            Ok(_) => {
                self.known_directories.remove(path);
                false
            }
            Err(_) => {
                if self.known_directories.remove(path) {
                    return true;
                }
                matches!(action, FILE_ACTION_REMOVED | FILE_ACTION_RENAMED_OLD_NAME)
                    && path.extension().is_none()
            }
        }
    }
}

struct Record {
    action: u32,
    name: OsString,
    next_offset: usize,
}

impl Record {
    fn parse(bytes: &[u8]) -> Option<Self> {
        let header = bytes.get(..RECORD_HEADER)?;
        let next_offset = u32::from_ne_bytes(header[0..4].try_into().ok()?) as usize;
        let action = u32::from_ne_bytes(header[4..8].try_into().ok()?);
        let name_bytes = u32::from_ne_bytes(header[8..12].try_into().ok()?) as usize;
        let end = RECORD_HEADER.checked_add(name_bytes)?;
        let name: Vec<u16> = bytes
            .get(RECORD_HEADER..end)?
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u16::from_ne_bytes(*pair))
            .collect();
        Some(Self {
            action,
            name: OsString::from_wide(&name),
            next_offset,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::temp::TempDir;
    use crate::watch::test_support::wait_for;
    use std::fs;
    use std::sync::mpsc;
    use std::time::Instant;

    #[test]
    fn stop_is_prompt() {
        let directory = TempDir::new().unwrap();
        let (sender, receiver) = mpsc::sync_channel(4096);
        let watcher = watch_project_into(directory.path(), false, sender).unwrap();
        let path = directory.path().join("file.gd");
        fs::write(&path, "one").unwrap();
        wait_for(&receiver, vec![(WatcherChangeKind::Created, path)]);
        let start = Instant::now();
        drop(watcher);
        let elapsed = start.elapsed();
        assert!(elapsed < Duration::from_secs(1), "{elapsed:?}");
    }

    #[test]
    fn drop_completes_with_full_channel() {
        let directory = TempDir::new().unwrap();
        let (sender, receiver) = mpsc::sync_channel(1);
        sender
            .send(ProxyEvent::Watcher(Ok(WatcherChange {
                kind: WatcherChangeKind::Rescan,
                path: directory.path().to_owned(),
            })))
            .unwrap();
        let watcher = watch_project_into(directory.path(), false, sender).unwrap();
        for index in 0..8 {
            fs::write(directory.path().join(format!("file{index}.gd")), "one").unwrap();
        }
        let start = Instant::now();
        drop(watcher);
        let elapsed = start.elapsed();
        assert!(elapsed < Duration::from_secs(2), "{elapsed:?}");
        drop(receiver);
    }

    #[test]
    fn skipped_directory_is_silent() {
        let directory = TempDir::new().unwrap();
        fs::create_dir(directory.path().join(".godot")).unwrap();
        fs::create_dir(directory.path().join("addons")).unwrap();
        let (sender, receiver) = mpsc::sync_channel(4096);
        let _watcher = watch_project_into(directory.path(), false, sender).unwrap();
        fs::write(directory.path().join(".godot").join("x.gd"), "one").unwrap();
        fs::write(directory.path().join("addons").join("y.gd"), "one").unwrap();
        assert!(receiver.recv_timeout(Duration::from_millis(500)).is_err());
        let sibling = directory.path().join("sibling.gd");
        fs::write(&sibling, "one").unwrap();
        wait_for(&receiver, vec![(WatcherChangeKind::Created, sibling)]);
    }
}
