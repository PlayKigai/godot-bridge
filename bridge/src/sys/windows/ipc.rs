//! The owner's request channel: a newline-delimited JSON server on a named
//! pipe whose security descriptor admits only the current user.
//!
//! The wire protocol is the one the Unix socket speaks, one JSON request line
//! in and one JSON response line out, so `status`, the `open-editor` hand-off
//! and DAP owner discovery are unchanged. Every read and write is overlapped
//! so that a deadline, or the stop event the handle sets when it is dropped,
//! can end a wait that a blocking handle would hold forever.

use std::io::{self, BufReader, Read};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_ACCESS_DENIED, ERROR_BROKEN_PIPE, ERROR_IO_PENDING,
    ERROR_NO_DATA, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, ERROR_PIPE_NOT_CONNECTED, GENERIC_READ,
    GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, WriteFile, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED,
    OPEN_EXISTING, PIPE_ACCESS_DUPLEX,
};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, WaitNamedPipeW, NAMED_PIPE_MODE,
    PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES,
    PIPE_WAIT,
};
use windows_sys::Win32::System::Threading::{
    CreateEventW, ResetEvent, SetEvent, WaitForMultipleObjects, INFINITE,
};
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};

use super::security::{current_user_descriptor, wide, SecurityDescriptor};
use crate::json::Value;
use crate::state::read_line_limited;

const SOCKET_CLIENT_CAP: usize = 16;
const SOCKET_TIMEOUT: Duration = Duration::from_secs(5);
const SOCKET_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const THREAD_STACK: usize = 256 * 1024;
const PIPE_BUFFER: u32 = 64 * 1024;
/// Only `PIPE_REJECT_REMOTE_CLIENTS` sets a bit; the byte stream, byte read
/// mode and blocking mode are all zero.
const PIPE_MODE: NAMED_PIPE_MODE =
    PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS;

/// Stops the server when dropped. A named pipe disappears with its last
/// instance, so there is nothing to unlink.
pub struct SocketHandle {
    stop: Arc<OwnedHandle>,
    accept: Option<JoinHandle<()>>,
    clients: Arc<Mutex<Vec<JoinHandle<()>>>>,
}

impl Drop for SocketHandle {
    fn drop(&mut self) {
        unsafe { SetEvent(self.stop.0) };
        if let Some(accept) = self.accept.take() {
            let _ = accept.join();
        }
        if let Ok(mut clients) = self.clients.lock() {
            for client in clients.drain(..) {
                let _ = client.join();
            }
        }
    }
}

/// Answer JSON requests on `path` until the returned handle is dropped.
pub fn serve_socket<F>(path: &Path, handler: F) -> io::Result<SocketHandle>
where
    F: Fn(Value) -> Value + Send + Sync + 'static,
{
    let name = wide(path.as_os_str());
    let security = current_user_descriptor()?;
    let first = create_instance(&name, &security, true)?;
    let stop = Arc::new(new_event()?);
    let clients = Arc::new(Mutex::new(Vec::new()));
    let count = Arc::new(AtomicUsize::new(0));
    let handler = Arc::new(handler);
    let accept = thread::Builder::new()
        .name("godot-bridge-socket".to_owned())
        .stack_size(THREAD_STACK)
        .spawn({
            let stop = Arc::clone(&stop);
            let clients = Arc::clone(&clients);
            let count = Arc::clone(&count);
            move || accept_loop(&name, &security, first, &stop, &handler, &clients, &count)
        })?;
    Ok(SocketHandle {
        stop,
        accept: Some(accept),
        clients,
    })
}

fn accept_loop<F>(
    name: &[u16],
    security: &SecurityDescriptor,
    first: OwnedHandle,
    stop: &Arc<OwnedHandle>,
    handler: &Arc<F>,
    clients: &Mutex<Vec<JoinHandle<()>>>,
    count: &Arc<AtomicUsize>,
) where
    F: Fn(Value) -> Value + Send + Sync + 'static,
{
    let mut instance = first;
    loop {
        match wait_for_connection(&instance, stop.0) {
            Ok(true) => {}
            Ok(false) => break,
            Err(error) => {
                crate::warn!("named pipe connect failed: {error}");
                break;
            }
        }
        let next = match create_instance(name, security, false) {
            Ok(next) => next,
            Err(error) => {
                crate::warn!("named pipe instance failed: {error}");
                break;
            }
        };
        let connected = std::mem::replace(&mut instance, next);
        if count.load(Ordering::Acquire) >= SOCKET_CLIENT_CAP {
            unsafe { DisconnectNamedPipe(connected.0) };
            continue;
        }
        let slot = Slot::taken(count);
        let handler = Arc::clone(handler);
        let stop = Arc::clone(stop);
        let client = thread::Builder::new()
            .name("godot-bridge-socket-client".to_owned())
            .stack_size(THREAD_STACK)
            .spawn(move || {
                let _slot = slot;
                serve_client(&connected, handler.as_ref(), stop.0);
            });
        if let Ok(client) = client {
            if let Ok(mut clients) = clients.lock() {
                clients.retain(|client| !client.is_finished());
                clients.push(client);
            }
        }
    }
}

/// One of the [`SOCKET_CLIENT_CAP`] slots, released however the client thread
/// ends, including a thread that failed to start.
struct Slot(Arc<AtomicUsize>);

impl Slot {
    fn taken(count: &Arc<AtomicUsize>) -> Self {
        count.fetch_add(1, Ordering::AcqRel);
        Self(Arc::clone(count))
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// `Ok(false)` means the stop event fired before a client arrived.
fn wait_for_connection(instance: &OwnedHandle, stop: HANDLE) -> io::Result<bool> {
    let event = new_event()?;
    let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
    overlapped.hEvent = event.0;
    if unsafe { ConnectNamedPipe(instance.0, &mut overlapped) } == 0 {
        let error = unsafe { GetLastError() };
        if error == ERROR_PIPE_CONNECTED {
            return Ok(true);
        }
        if error != ERROR_IO_PENDING {
            return Err(io::Error::from_raw_os_error(error as i32));
        }
    }
    let handles = [event.0, stop];
    let waited = unsafe { WaitForMultipleObjects(2, handles.as_ptr(), 0, INFINITE) };
    if waited != WAIT_OBJECT_0 {
        cancel(instance.0, &overlapped);
        return if waited == WAIT_OBJECT_0 + 1 {
            Ok(false)
        } else {
            Err(io::Error::last_os_error())
        };
    }
    let mut transferred = 0u32;
    if unsafe { GetOverlappedResult(instance.0, &overlapped, &mut transferred, 0) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(true)
}

fn serve_client<F>(pipe: &OwnedHandle, handler: &F, stop: HANDLE)
where
    F: Fn(Value) -> Value,
{
    let _ = serve_connection(pipe.0, handler, stop);
    // Every response has already awaited its overlapped `WriteFile`, so there
    // is nothing left to flush and a client that never reads cannot hold this.
    unsafe { DisconnectNamedPipe(pipe.0) };
}

fn serve_connection<F>(pipe: HANDLE, handler: &F, stop: HANDLE) -> io::Result<()>
where
    F: Fn(Value) -> Value,
{
    let write_event = new_event()?;
    let mut reader = BufReader::new(PipeReader::new(pipe, Some(stop))?);
    let mut partial = Vec::new();
    loop {
        reader.get_mut().deadline = Instant::now() + SOCKET_IDLE_TIMEOUT;
        let Some(line) = read_line_limited(&mut reader, &mut partial)? else {
            return Ok(());
        };
        let response = match crate::json::from_slice(&line) {
            Ok(request) => handler(request),
            Err(_) => crate::json!({"error": "invalid json"}),
        };
        let mut bytes = crate::json::to_vec(&response);
        bytes.push(b'\n');
        write_line(pipe, write_event.0, &bytes, Instant::now() + SOCKET_TIMEOUT)?;
    }
}

/// Send one JSON request to the owner listening on `path` and read its reply.
pub fn socket_request(path: &Path, req: &Value, timeout: Duration) -> io::Result<Value> {
    let deadline = Instant::now() + timeout;
    let name = wide(path.as_os_str());
    let pipe = open_pipe(&name, deadline)?;
    let write_event = new_event()?;
    let mut bytes = crate::json::to_vec(req);
    bytes.push(b'\n');
    write_line(pipe.0, write_event.0, &bytes, deadline)?;
    let mut reader = BufReader::new(PipeReader::new(pipe.0, None)?);
    reader.get_mut().deadline = deadline;
    let mut partial = Vec::new();
    let line = read_line_limited(&mut reader, &mut partial)?
        .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "socket closed"))?;
    crate::json::from_slice(&line).map_err(io::Error::other)
}

/// The server holds one free instance at a time, so concurrent clients race
/// for it and a waiter that loses the race has to wait again.
fn open_pipe(name: &[u16], deadline: Instant) -> io::Result<OwnedHandle> {
    loop {
        match open_pipe_once(name) {
            Err(error) if error.raw_os_error() == Some(ERROR_PIPE_BUSY as i32) => {
                let remaining = remaining_ms(deadline);
                if remaining == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "every named pipe instance is busy",
                    ));
                }
                unsafe { WaitNamedPipeW(name.as_ptr(), remaining) };
            }
            other => return other,
        }
    }
}

fn open_pipe_once(name: &[u16]) -> io::Result<OwnedHandle> {
    let handle = unsafe {
        CreateFileW(
            name.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            0,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_OVERLAPPED,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    Ok(OwnedHandle(handle))
}

struct PipeReader {
    pipe: HANDLE,
    event: OwnedHandle,
    stop: Option<HANDLE>,
    deadline: Instant,
}

impl PipeReader {
    fn new(pipe: HANDLE, stop: Option<HANDLE>) -> io::Result<Self> {
        Ok(Self {
            pipe,
            event: new_event()?,
            stop,
            deadline: Instant::now(),
        })
    }
}

impl Read for PipeReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let len = buf.len().min(PIPE_BUFFER as usize) as u32;
        let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
        overlapped.hEvent = self.event.0;
        unsafe { ResetEvent(self.event.0) };
        if unsafe {
            ReadFile(
                self.pipe,
                buf.as_mut_ptr(),
                len,
                std::ptr::null_mut(),
                &mut overlapped,
            )
        } == 0
        {
            let error = unsafe { GetLastError() };
            if is_disconnect(error) {
                return Ok(0);
            }
            if error != ERROR_IO_PENDING {
                return Err(io::Error::from_raw_os_error(error as i32));
            }
        }
        match wait_io(self.event.0, self.stop, self.deadline)? {
            Wait::Ready => {}
            Wait::Expired => {
                cancel(self.pipe, &overlapped);
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "socket read timed out",
                ));
            }
            Wait::Stop => {
                cancel(self.pipe, &overlapped);
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionAborted,
                    "socket server stopped",
                ));
            }
        }
        let mut transferred = 0u32;
        if unsafe { GetOverlappedResult(self.pipe, &overlapped, &mut transferred, 0) } == 0 {
            let error = unsafe { GetLastError() };
            if is_disconnect(error) {
                return Ok(0);
            }
            return Err(io::Error::from_raw_os_error(error as i32));
        }
        Ok(transferred as usize)
    }
}

fn write_line(pipe: HANDLE, event: HANDLE, bytes: &[u8], deadline: Instant) -> io::Result<()> {
    let mut written = 0usize;
    while written < bytes.len() {
        let chunk = &bytes[written..];
        let len = chunk.len().min(PIPE_BUFFER as usize) as u32;
        let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
        overlapped.hEvent = event;
        unsafe { ResetEvent(event) };
        if unsafe {
            WriteFile(
                pipe,
                chunk.as_ptr(),
                len,
                std::ptr::null_mut(),
                &mut overlapped,
            )
        } == 0
        {
            let error = unsafe { GetLastError() };
            if error != ERROR_IO_PENDING {
                return Err(io::Error::from_raw_os_error(error as i32));
            }
        }
        if wait_io(event, None, deadline)? != Wait::Ready {
            cancel(pipe, &overlapped);
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "socket write timed out",
            ));
        }
        let mut transferred = 0u32;
        if unsafe { GetOverlappedResult(pipe, &overlapped, &mut transferred, 0) } == 0 {
            return Err(io::Error::last_os_error());
        }
        if transferred == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "socket write made no progress",
            ));
        }
        written += transferred as usize;
    }
    Ok(())
}

#[derive(PartialEq, Eq)]
enum Wait {
    Ready,
    Expired,
    Stop,
}

fn wait_io(event: HANDLE, stop: Option<HANDLE>, deadline: Instant) -> io::Result<Wait> {
    let mut handles = [event, event];
    let count = match stop {
        Some(stop) => {
            handles[1] = stop;
            2
        }
        None => 1,
    };
    let remaining = remaining_ms(deadline);
    if remaining == 0 {
        return Ok(Wait::Expired);
    }
    match unsafe { WaitForMultipleObjects(count, handles.as_ptr(), 0, remaining) } {
        WAIT_OBJECT_0 => Ok(Wait::Ready),
        WAIT_TIMEOUT => Ok(Wait::Expired),
        waited if waited == WAIT_OBJECT_0 + 1 => Ok(Wait::Stop),
        _ => Err(io::Error::last_os_error()),
    }
}

/// `INFINITE` is `u32::MAX`, so a deadline that far away is clamped rather
/// than turned into a wait that never ends.
fn remaining_ms(deadline: Instant) -> u32 {
    deadline
        .saturating_duration_since(Instant::now())
        .as_millis()
        .min(u128::from(INFINITE - 1)) as u32
}

fn cancel(pipe: HANDLE, overlapped: &OVERLAPPED) {
    let mut transferred = 0u32;
    unsafe {
        CancelIoEx(pipe, overlapped);
        GetOverlappedResult(pipe, overlapped, &mut transferred, 1);
    }
}

fn is_disconnect(error: u32) -> bool {
    error == ERROR_BROKEN_PIPE || error == ERROR_PIPE_NOT_CONNECTED || error == ERROR_NO_DATA
}

fn create_instance(
    name: &[u16],
    security: &SecurityDescriptor,
    first: bool,
) -> io::Result<OwnedHandle> {
    let attributes = security.attributes();
    let mut open_mode = PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED;
    if first {
        open_mode |= FILE_FLAG_FIRST_PIPE_INSTANCE;
    }
    let handle = unsafe {
        CreateNamedPipeW(
            name.as_ptr(),
            open_mode,
            PIPE_MODE,
            PIPE_UNLIMITED_INSTANCES,
            PIPE_BUFFER,
            PIPE_BUFFER,
            0,
            &attributes,
        )
    };
    if handle != INVALID_HANDLE_VALUE {
        return Ok(OwnedHandle(handle));
    }
    let error = io::Error::last_os_error();
    match error.raw_os_error() {
        Some(code)
            if first && (code == ERROR_ACCESS_DENIED as i32 || code == ERROR_PIPE_BUSY as i32) =>
        {
            Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                "another owner already listens on this named pipe",
            ))
        }
        _ => Err(error),
    }
}

struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

// A kernel handle belongs to the process, not to the thread that made it.
unsafe impl Send for OwnedHandle {}
unsafe impl Sync for OwnedHandle {}

fn new_event() -> io::Result<OwnedHandle> {
    let handle = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
    if handle.is_null() {
        return Err(io::Error::last_os_error());
    }
    Ok(OwnedHandle(handle))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sys::windows::security::{current_user_sid, sid_to_string, LocalBlock};
    use std::fs::OpenOptions;
    use std::io::{BufRead, Write};
    use std::path::PathBuf;
    use std::sync::atomic::AtomicUsize;
    use windows_sys::Win32::Security::Authorization::{GetSecurityInfo, SE_KERNEL_OBJECT};
    use windows_sys::Win32::Security::{
        GetAce, ACCESS_ALLOWED_ACE, ACL, DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
    };

    use crate::sys::windows::security::ACCESS_ALLOWED;

    fn unique_pipe() -> PathBuf {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let pid = std::process::id();
        let count = COUNTER.fetch_add(1, Ordering::Relaxed);
        PathBuf::from(format!(r"\\.\pipe\godot-bridge-test-{pid}-{count}"))
    }

    fn echo_server(path: &Path) -> SocketHandle {
        serve_socket(path, |request| {
            let cmd = request
                .get("cmd")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            crate::json!({"echo": (cmd)})
        })
        .unwrap()
    }

    fn ask(path: &Path, cmd: &str) -> io::Result<Value> {
        socket_request(
            path,
            &crate::json!({"cmd": (cmd.to_owned())}),
            Duration::from_secs(5),
        )
    }

    #[test]
    fn socket_status_round_trip() {
        let path = unique_pipe();
        let handle = echo_server(&path);

        let response = ask(&path, "status").unwrap();
        assert_eq!(response.get("echo").and_then(Value::as_str), Some("status"));

        let client = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let mut client = io::BufReader::new(client);
        for cmd in ["handoff", "status"] {
            let line = format!("{{\"cmd\":\"{cmd}\"}}\r\n");
            client.get_mut().write_all(line.as_bytes()).unwrap();
            let mut reply = String::new();
            client.read_line(&mut reply).unwrap();
            assert!(reply.contains(cmd), "{reply}");
        }
        drop(client);

        drop(handle);
        let error = ask(&path, "status").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn second_server_on_same_name_fails() {
        let path = unique_pipe();
        let _handle = echo_server(&path);
        let Err(error) = serve_socket(&path, |request| request) else {
            panic!("a second server on the same name must fail");
        };
        assert_eq!(error.kind(), io::ErrorKind::AddrInUse);
    }

    #[test]
    fn oversized_request_line_is_rejected() {
        let path = unique_pipe();
        let _handle = echo_server(&path);

        let mut client = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let payload = vec![b'x'; 70 * 1024];
        let refused = match client.write_all(&payload) {
            Err(_) => true,
            Ok(()) => {
                let mut reply = Vec::new();
                client.read_to_end(&mut reply).is_err() || reply.is_empty()
            }
        };
        drop(client);
        assert!(refused, "the server answered a 70 KiB line");

        let response = ask(&path, "status").unwrap();
        assert_eq!(response.get("echo").and_then(Value::as_str), Some("status"));
    }

    #[test]
    fn client_timeout_when_server_never_answers() {
        let path = unique_pipe();
        let _handle = serve_socket(&path, |request| {
            std::thread::sleep(Duration::from_secs(2));
            request
        })
        .unwrap();
        let error = socket_request(
            &path,
            &crate::json!({"cmd": "status"}),
            Duration::from_millis(200),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }

    #[test]
    fn concurrent_clients_are_served() {
        let path = unique_pipe();
        let _handle = echo_server(&path);
        let mut askers = Vec::new();
        for index in 0..4 {
            let path = path.clone();
            askers.push(thread::spawn(move || {
                let cmd = format!("status-{index}");
                let response = ask(&path, &cmd).unwrap();
                assert_eq!(
                    response.get("echo").and_then(Value::as_str),
                    Some(cmd.as_str())
                );
            }));
        }
        for asker in askers {
            asker.join().unwrap();
        }
    }

    #[test]
    fn unread_client_does_not_block_shutdown() {
        let path = unique_pipe();
        let (served, answered) = std::sync::mpsc::channel();
        let handle = serve_socket(&path, move |request| {
            let _ = served.send(());
            request
        })
        .unwrap();

        let mut client = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        client.write_all(b"{\"cmd\":\"status\"}\n").unwrap();
        answered
            .recv_timeout(Duration::from_secs(5))
            .expect("the server answers the request");

        let start = Instant::now();
        drop(handle);
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_secs(2),
            "shutdown took {elapsed:?}"
        );
        drop(client);

        let fresh = unique_pipe();
        let _server = echo_server(&fresh);
        let response = ask(&fresh, "status").unwrap();
        assert_eq!(response.get("echo").and_then(Value::as_str), Some("status"));
    }

    #[test]
    fn pipe_dacl_is_current_user_only() {
        let name = wide(unique_pipe().as_os_str());
        let security = current_user_descriptor().unwrap();
        let instance = create_instance(&name, &security, true).unwrap();

        let mut dacl: *mut ACL = std::ptr::null_mut();
        let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        let read = unsafe {
            GetSecurityInfo(
                instance.0,
                SE_KERNEL_OBJECT,
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut dacl,
                std::ptr::null_mut(),
                &mut descriptor,
            )
        };
        assert_eq!(read, 0);
        let descriptor = LocalBlock(descriptor);
        assert_eq!(unsafe { (*dacl).AceCount }, 1);

        let mut ace = std::ptr::null_mut();
        assert_ne!(unsafe { GetAce(dacl, 0, &mut ace) }, 0);
        let ace = ace.cast::<ACCESS_ALLOWED_ACE>();
        assert_eq!(unsafe { (*ace).Header.AceType }, ACCESS_ALLOWED);
        let sid = unsafe { std::ptr::addr_of!((*ace).SidStart) }
            .cast_mut()
            .cast();
        assert_eq!(
            sid_to_string(sid).unwrap(),
            current_user_sid().unwrap(),
            "the pipe grants someone other than the current user"
        );
        drop(descriptor);
    }
}
