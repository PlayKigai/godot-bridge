//! The owner's request socket: a newline-delimited JSON server on a Unix
//! socket in the runtime directory.

use std::io::{self, BufReader, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::json::Value;
use crate::state::read_line_limited;
use crate::sys::remove_socket;

const SOCKET_CLIENT_CAP: usize = 16;
const SOCKET_TIMEOUT: Duration = Duration::from_secs(5);
const SOCKET_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// Stops the server and removes the socket when dropped.
pub struct SocketHandle {
    path: PathBuf,
    stop: Arc<AtomicBool>,
    listener: Option<JoinHandle<()>>,
    clients: Arc<Mutex<Vec<JoinHandle<()>>>>,
}

impl Drop for SocketHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = UnixStream::connect(&self.path);
        if let Some(listener) = self.listener.take() {
            let _ = listener.join();
        }
        if let Ok(mut clients) = self.clients.lock() {
            for client in clients.drain(..) {
                let _ = client.join();
            }
        }
        let _ = remove_socket(&self.path);
    }
}

/// Answer JSON requests on `path` until the returned handle is dropped.
pub fn serve_socket<F>(path: &Path, handler: F) -> io::Result<SocketHandle>
where
    F: Fn(Value) -> Value + Send + Sync + 'static,
{
    let path = path.to_path_buf();
    let temporary = path.with_extension("tmp");
    remove_socket(&temporary)?;
    let listener = UnixListener::bind(&temporary)?;
    std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o600))?;
    std::fs::rename(&temporary, &path)?;
    let handler = Arc::new(handler);
    let stop = Arc::new(AtomicBool::new(false));
    let clients = Arc::new(Mutex::new(Vec::<JoinHandle<()>>::new()));
    let count = Arc::new(AtomicUsize::new(0));
    let stop_for_thread = Arc::clone(&stop);
    let clients_for_thread = Arc::clone(&clients);
    let count_for_thread = Arc::clone(&count);
    let listener_thread = thread::Builder::new()
        .name("godot-bridge-socket".to_owned())
        .stack_size(256 * 1024)
        .spawn(move || {
            while !stop_for_thread.load(Ordering::Acquire) {
                let (stream, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(_) => break,
                };
                if count_for_thread.load(Ordering::Acquire) >= SOCKET_CLIENT_CAP {
                    continue;
                }
                count_for_thread.fetch_add(1, Ordering::AcqRel);
                let handler = Arc::clone(&handler);
                let stop = Arc::clone(&stop_for_thread);
                let count = Arc::clone(&count_for_thread);
                let client = thread::Builder::new()
                    .name("godot-bridge-socket-client".to_owned())
                    .stack_size(256 * 1024)
                    .spawn(move || {
                        handle_client(stream, handler, stop);
                        count.fetch_sub(1, Ordering::AcqRel);
                    });
                if let Ok(client) = client {
                    if let Ok(mut clients) = clients_for_thread.lock() {
                        clients.retain(|client| !client.is_finished());
                        clients.push(client);
                    }
                } else {
                    count_for_thread.fetch_sub(1, Ordering::AcqRel);
                }
            }
        })?;
    Ok(SocketHandle {
        path,
        stop,
        listener: Some(listener_thread),
        clients,
    })
}

fn handle_client<F>(stream: UnixStream, handler: Arc<F>, stop: Arc<AtomicBool>)
where
    F: Fn(Value) -> Value + Send + Sync + 'static,
{
    let _ = stream.set_write_timeout(Some(SOCKET_TIMEOUT));
    let reader_stream = match stream.try_clone() {
        Ok(reader_stream) => reader_stream,
        Err(_) => return,
    };
    if reader_stream.set_nonblocking(true).is_err() {
        return;
    }
    let mut reader = BufReader::new(reader_stream);
    let mut write = stream;
    let mut partial = Vec::new();
    let mut idle_deadline = Instant::now() + SOCKET_IDLE_TIMEOUT;
    loop {
        if stop.load(Ordering::Acquire) {
            break;
        }
        if reader.buffer().is_empty() {
            match poll_for_read(reader.get_ref().as_raw_fd(), idle_deadline) {
                Ok(true) => {}
                Ok(false) | Err(_) => break,
            }
        }
        let line = match read_line_limited(&mut reader, &mut partial) {
            Ok(Some(line)) => line,
            Ok(None) => break,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                continue;
            }
            Err(_) => break,
        };
        idle_deadline = Instant::now() + SOCKET_IDLE_TIMEOUT;
        let response = match crate::json::from_slice(&line) {
            Ok(request) => handler(request),
            Err(_) => crate::json!({"error": "invalid json"}),
        };
        let mut bytes = crate::json::to_vec(&response);
        bytes.push(b'\n');
        if write.write_all(&bytes).is_err() {
            break;
        }
    }
}

fn poll_for_read(fd: RawFd, deadline: Instant) -> io::Result<bool> {
    loop {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return Ok(false);
        };
        if remaining.is_zero() {
            return Ok(false);
        }
        let timeout = remaining.as_millis().min(i32::MAX as u128).max(1) as i32;
        let mut descriptor = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let result = unsafe { libc::poll(&mut descriptor, 1, timeout) };
        if result >= 0 {
            return Ok(result != 0);
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::EINTR) {
            return Err(error);
        }
    }
}

/// Send one JSON request to the owner listening on `path` and read its reply.
pub fn socket_request(path: &Path, req: &Value, timeout: Duration) -> io::Result<Value> {
    let stream = UnixStream::connect(path)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    let mut write = stream.try_clone()?;
    let mut reader = BufReader::new(stream);
    let mut bytes = crate::json::to_vec(req);
    bytes.push(b'\n');
    write.write_all(&bytes)?;
    let mut partial = Vec::new();
    let line = read_line_limited(&mut reader, &mut partial)?
        .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "socket closed"))?;
    crate::json::from_slice(&line).map_err(io::Error::other)
}
