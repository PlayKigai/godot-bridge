//! Runtime state files, locks, and status sockets.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::future::Future;
use std::io;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::task::JoinHandle;

fn not_found_ok(result: io::Result<()>) -> io::Result<()> {
    match result {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

/// Returns the private directory used for bridge runtime files.
pub fn runtime_dir() -> io::Result<PathBuf> {
    let path = match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) => PathBuf::from(dir).join("godot-bridge"),
        None => PathBuf::from(format!("/tmp/godot-bridge-{}", unsafe { libc::geteuid() })),
    };
    std::fs::create_dir_all(&path)?;
    let mut permissions = std::fs::metadata(&path)?.permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(&path, permissions)?;
    Ok(path)
}

/// The four runtime files belonging to one project.
pub struct ProjectFiles {
    pub lock: PathBuf,
    pub sock: PathBuf,
    pub state: PathBuf,
    pub dap_lock: PathBuf,
}

impl ProjectFiles {
    /// Builds runtime file paths from a canonical project path.
    pub fn new(project: &Path) -> io::Result<Self> {
        let hash = blake3::hash(project.to_string_lossy().as_bytes())
            .to_hex()
            .to_string();
        let prefix = runtime_dir()?.join(&hash[..16]);
        Ok(Self {
            lock: prefix.with_extension("lock"),
            sock: prefix.with_extension("sock"),
            state: prefix.with_extension("json"),
            dap_lock: prefix.with_extension("dap.lock"),
        })
    }
}

/// An exclusively held non-blocking runtime lock.
pub struct LockGuard {
    file: std::fs::File,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

/// Tries to take an exclusive lock without waiting.
pub fn try_lock(path: &Path) -> io::Result<Option<LockGuard>> {
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(path)?;
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result == 0 {
        Ok(Some(LockGuard { file }))
    } else if io::Error::last_os_error().raw_os_error() == Some(libc::EWOULDBLOCK) {
        Ok(None)
    } else {
        Err(io::Error::last_os_error())
    }
}

/// The lifecycle state published by a bridge owner.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct State {
    pub version: u32,
    pub project: String,
    pub status: Status,
    pub mode: Mode,
    pub godot_pid: Option<u32>,
    pub godot_pgid: Option<u32>,
    pub lsp_port: Option<u16>,
    pub dap_port: Option<u16>,
    pub owner_pid: Option<u32>,
    pub owner_start_ticks: Option<u64>,
    pub godot_start_ticks: Option<u64>,
    pub started_at: String,
    pub bridge_version: String,
}

/// Bridge startup and recovery status.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Starting,
    Ready,
    Recovering,
}

/// The editor process mode.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Headless,
    Gui,
    Unmanaged,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HandoffDecision {
    Reject(&'static str),
    AlreadyGui,
    Swap,
}

pub fn handoff_decision(state: &State) -> HandoffDecision {
    match state.status {
        Status::Starting => HandoffDecision::Reject("editor is starting"),
        Status::Recovering => HandoffDecision::Reject("editor is recovering"),
        Status::Ready => match state.mode {
            Mode::Unmanaged => HandoffDecision::Reject("editor is unmanaged"),
            Mode::Gui => HandoffDecision::AlreadyGui,
            Mode::Headless => HandoffDecision::Swap,
        },
    }
}

pub fn detached_gui_state(project: &Path, status: Status) -> State {
    State {
        version: 1,
        project: project.to_string_lossy().into_owned(),
        status,
        mode: Mode::Gui,
        godot_pid: None,
        godot_pgid: None,
        lsp_port: None,
        dap_port: None,
        owner_pid: None,
        owner_start_ticks: None,
        godot_start_ticks: None,
        started_at: time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_default(),
        bridge_version: env!("CARGO_PKG_VERSION").to_owned(),
    }
}

pub fn set_owner_identity(state: &mut State) {
    let pid = std::process::id();
    state.owner_pid = Some(pid);
    state.owner_start_ticks = start_ticks(pid);
}

pub fn clear_owner_identity(state: &mut State) {
    state.owner_pid = None;
    state.owner_start_ticks = None;
}

pub fn gui_process_alive(state: &State) -> bool {
    state.mode == Mode::Gui
        && state
            .godot_pid
            .zip(state.godot_start_ticks)
            .is_some_and(|(pid, ticks)| pid_alive_with_ticks(pid, ticks))
}

pub fn remove_lock_file(path: &Path) {
    let _ = std::fs::remove_file(path);
}

/// Atomically writes state with private file permissions.
pub fn write_state(path: &Path, state: &State) -> io::Result<()> {
    let temp = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec(state).map_err(io::Error::other)?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(&temp)?;
    use std::io::Write;
    file.write_all(&bytes)?;
    file.sync_all()?;
    std::fs::rename(temp, path)
}

/// Reads state, returning `None` when the state file is absent.
pub fn read_state(path: &Path) -> io::Result<Option<State>> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(io::Error::other),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// Reads process start ticks from field 22 of `/proc/<pid>/stat`.
pub fn start_ticks(pid: u32) -> Option<u64> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_name = text.rsplit_once(')')?.1;
    after_name
        .split_whitespace()
        .nth(19)
        .and_then(|ticks| ticks.parse().ok())
}

/// Checks that a process exists and is the process identified by its start ticks.
pub fn pid_alive_with_ticks(pid: u32, ticks: u64) -> bool {
    start_ticks(pid) == Some(ticks)
}

/// Removes state and socket files when the recorded owner is no longer the same process.
pub fn remove_if_stale(files: &ProjectFiles) -> io::Result<bool> {
    let Some(state) = read_state(&files.state)? else {
        return Ok(false);
    };
    let stale = match (state.owner_pid, state.owner_start_ticks) {
        (Some(pid), Some(ticks)) => !pid_alive_with_ticks(pid, ticks),
        (Some(_), None) => true,
        (None, _) => false,
    };
    if !stale {
        return Ok(false);
    }
    not_found_ok(std::fs::remove_file(&files.state))?;
    not_found_ok(std::fs::remove_file(&files.sock))?;
    Ok(true)
}

/// A running Unix socket server; dropping it stops the server and removes its path.
pub struct SocketHandle {
    path: PathBuf,
    task: JoinHandle<()>,
}

impl Drop for SocketHandle {
    fn drop(&mut self) {
        self.task.abort();
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Serves concurrent newline-delimited JSON requests over a Unix socket.
pub async fn serve_socket<F, Fut>(path: impl AsRef<Path>, handler: F) -> io::Result<SocketHandle>
where
    F: Fn(Value) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Value> + Send + 'static,
{
    let path = path.as_ref().to_path_buf();
    not_found_ok(std::fs::remove_file(&path))?;
    let listener = UnixListener::bind(&path)?;
    let handler = Arc::new(handler);
    let task = tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let handler = Arc::clone(&handler);
            tokio::spawn(handle_client(stream, handler));
        }
    });
    Ok(SocketHandle { path, task })
}

/// Returns the standard response for an unsupported socket command.
pub fn unknown_command() -> Value {
    serde_json::json!({"error": "unknown cmd"})
}

async fn handle_client<F, Fut>(stream: UnixStream, handler: Arc<F>)
where
    F: Fn(Value) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Value> + Send + 'static,
{
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let response = match serde_json::from_str::<Value>(&line) {
            Ok(request) if request.get("cmd").and_then(Value::as_str).is_some() => {
                let command = request
                    .get("cmd")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if command == "status" || command == "handoff" {
                    match tokio::time::timeout(Duration::from_secs(5), handler(request)).await {
                        Ok(response) => response,
                        Err(_) => serde_json::json!({"error": "request timed out"}),
                    }
                } else {
                    unknown_command()
                }
            }
            Ok(_) => unknown_command(),
            Err(_) => serde_json::json!({"error": "invalid json"}),
        };
        let Ok(mut bytes) = serde_json::to_vec(&response) else {
            break;
        };
        bytes.push(b'\n');
        if write.write_all(&bytes).await.is_err() {
            break;
        }
    }
}

/// Sends one newline-delimited JSON request and waits for its response.
pub async fn socket_request(
    path: impl AsRef<Path>,
    req: &Value,
    timeout: Duration,
) -> io::Result<Value> {
    tokio::time::timeout(timeout, async {
        let stream = UnixStream::connect(path).await?;
        let (read, mut write) = stream.into_split();
        let mut bytes = serde_json::to_vec(req).map_err(io::Error::other)?;
        bytes.push(b'\n');
        write.write_all(&bytes).await?;
        let mut lines = BufReader::new(read).lines();
        let line = lines
            .next_line()
            .await?
            .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "socket closed"))?;
        serde_json::from_str(&line).map_err(io::Error::other)
    })
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "socket request timed out"))?
}

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn files(dir: &Path) -> ProjectFiles {
        ProjectFiles {
            lock: dir.join("project.lock"),
            sock: dir.join("project.sock"),
            state: dir.join("project.json"),
            dap_lock: dir.join("project.dap.lock"),
        }
    }

    fn state(owner_pid: Option<u32>, owner_start_ticks: Option<u64>) -> State {
        State {
            version: 1,
            project: "/project".to_string(),
            status: Status::Starting,
            mode: Mode::Headless,
            godot_pid: None,
            godot_pgid: None,
            lsp_port: None,
            dap_port: None,
            owner_pid,
            owner_start_ticks,
            godot_start_ticks: None,
            started_at: "2026-09-04T00:00:00Z".to_string(),
            bridge_version: "0.1.0".to_string(),
        }
    }

    #[test]
    fn second_lock_is_rejected() {
        let dir = tempdir().unwrap();
        let first = try_lock(&dir.path().join("lock")).unwrap().unwrap();
        assert!(try_lock(&dir.path().join("lock")).unwrap().is_none());
        drop(first);
        assert!(try_lock(&dir.path().join("lock")).unwrap().is_some());
    }

    #[test]
    fn dead_owner_state_is_removed() {
        let dir = tempdir().unwrap();
        let project_files = files(dir.path());
        write_state(&project_files.state, &state(Some(u32::MAX), Some(1))).unwrap();
        std::fs::write(&project_files.sock, b"stale").unwrap();
        assert!(remove_if_stale(&project_files).unwrap());
        assert!(!project_files.state.exists());
        assert!(!project_files.sock.exists());
    }

    #[test]
    fn reused_pid_is_ignored_by_ticks() {
        let dir = tempdir().unwrap();
        let project_files = files(dir.path());
        let pid = std::process::id();
        let ticks = start_ticks(pid).unwrap();
        write_state(&project_files.state, &state(Some(pid), Some(ticks + 1))).unwrap();
        assert!(remove_if_stale(&project_files).unwrap());
    }

    #[test]
    fn starting_state_serializes_nullable_fields_as_null() {
        let value = serde_json::to_value(state(None, None)).unwrap();
        assert!(value["godot_pid"].is_null());
        assert!(value["lsp_port"].is_null());
        assert_eq!(value["status"], "starting");
    }

    #[test]
    fn handoff_rejects_starting_and_recovering_states() {
        let mut current = state(None, None);
        assert_eq!(
            handoff_decision(&current),
            HandoffDecision::Reject("editor is starting")
        );
        current.status = Status::Recovering;
        assert_eq!(
            handoff_decision(&current),
            HandoffDecision::Reject("editor is recovering")
        );
    }

    #[test]
    fn handoff_rejects_unmanaged_and_accepts_gui() {
        let mut current = state(None, None);
        current.status = Status::Ready;
        current.mode = Mode::Unmanaged;
        assert_eq!(
            handoff_decision(&current),
            HandoffDecision::Reject("editor is unmanaged")
        );
        current.mode = Mode::Gui;
        assert_eq!(handoff_decision(&current), HandoffDecision::AlreadyGui);
        current.mode = Mode::Headless;
        assert_eq!(handoff_decision(&current), HandoffDecision::Swap);
    }

    #[test]
    fn handoff_dap_lock_is_rejected_when_held() {
        let dir = tempdir().unwrap();
        let first = try_lock(&dir.path().join("dap.lock")).unwrap().unwrap();
        assert!(try_lock(&dir.path().join("dap.lock")).unwrap().is_none());
        drop(first);
    }

    #[test]
    fn detached_gui_state_has_null_owner_fields() {
        let value =
            serde_json::to_value(detached_gui_state(Path::new("/project"), Status::Starting))
                .unwrap();
        assert_eq!(value["mode"], "gui");
        assert_eq!(value["status"], "starting");
        assert!(value["owner_pid"].is_null());
        assert!(value["owner_start_ticks"].is_null());
        assert!(value["godot_pid"].is_null());
        assert!(value["lsp_port"].is_null());
        assert!(value["dap_port"].is_null());
    }

    #[tokio::test]
    async fn socket_status_round_trip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("status.sock");
        let handle = serve_socket(&path, |request| async move {
            if request["cmd"] == "status" {
                serde_json::json!({"status": "ready"})
            } else {
                serde_json::json!({"error": "unexpected"})
            }
        })
        .await
        .unwrap();
        let response = socket_request(
            &path,
            &serde_json::json!({"cmd": "status"}),
            Duration::from_secs(1),
        )
        .await
        .unwrap();
        assert_eq!(response["status"], "ready");
        drop(handle);
        assert!(!path.exists());
    }
}
