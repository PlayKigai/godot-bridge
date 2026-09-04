use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;

const SOCKET_CLIENT_CAP: usize = 16;
const SOCKET_LINE_CAP: usize = 64 * 1024;
const SOCKET_TIMEOUT: Duration = Duration::from_secs(5);

fn not_found_ok(result: io::Result<()>) -> io::Result<()> {
    match result {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

pub fn runtime_dir() -> io::Result<PathBuf> {
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) => ensure_private_dir(&PathBuf::from(dir).join("godot-bridge")),
        None => fallback_runtime_dir(),
    }
}

pub fn fallback_runtime_dir() -> io::Result<PathBuf> {
    ensure_private_dir(&PathBuf::from(format!(
        "/tmp/godot-bridge-{}",
        effective_uid()
    )))
}

fn ensure_private_dir(path: &Path) -> io::Result<PathBuf> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => validate_private_dir(path)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let mut builder = std::fs::DirBuilder::new();
            builder.mode(0o700);
            builder.create(path)?;
            validate_private_dir(path)?;
        }
        Err(error) => return Err(error),
    }
    let _directory = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(path)
        .map_err(|error| {
            io::Error::new(
                error.kind(),
                format!(
                    "cannot securely open runtime directory {}: {error}",
                    path.display()
                ),
            )
        })?;
    Ok(path.to_path_buf())
}

fn validate_private_dir(path: &Path) -> io::Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(io::Error::other(format!(
            "runtime directory {} is not a directory without symlinks",
            path.display()
        )));
    }
    if metadata.uid() != effective_uid() || metadata.mode() & 0o777 != 0o700 {
        return Err(io::Error::other(format!(
            "runtime directory {} must be owned by the current user with mode 0700",
            path.display()
        )));
    }
    Ok(())
}

pub struct ProjectFiles {
    pub lock: PathBuf,
    pub sock: PathBuf,
    pub state: PathBuf,
    pub dap_lock: PathBuf,
}

impl ProjectFiles {
    pub fn new(project: &Path) -> io::Result<Self> {
        let hash = crate::fnv::hash_hex(project.to_string_lossy().as_bytes());
        let mut runtime = runtime_dir()?;
        let socket = runtime.join(format!("{hash}.sock"));
        if socket.as_os_str().len() > 100 {
            crate::warn!(
                "socket path {} is too long; using fallback runtime directory",
                socket.display()
            );
            runtime = fallback_runtime_dir()?;
        }
        let prefix = runtime.join(&hash);
        Ok(Self {
            lock: prefix.with_extension("lock"),
            sock: prefix.with_extension("sock"),
            state: prefix.with_extension("json"),
            dap_lock: prefix.with_extension("dap.lock"),
        })
    }
}

fn effective_uid() -> u32 {
    unsafe { libc::geteuid() }
}

pub struct LockGuard {
    file: std::fs::File,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
    }
}

pub fn try_lock(path: &Path) -> io::Result<Option<LockGuard>> {
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
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

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Starting,
    Ready,
    Recovering,
}

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
        started_at: crate::clock::now_rfc3339(),
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

pub fn matches_project(state: &State, project: &Path) -> bool {
    state.project == project.to_string_lossy()
}

pub fn gui_process_alive(state: &State) -> bool {
    state.mode == Mode::Gui
        && state
            .godot_pid
            .zip(state.godot_start_ticks)
            .is_some_and(|(pid, ticks)| pid_alive_with_ticks(pid, ticks))
}

pub fn write_state(path: &Path, state: &State) -> io::Result<()> {
    let temp = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec(state).map_err(io::Error::other)?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&temp)?;
    use std::io::Write;
    file.write_all(&bytes)?;
    file.sync_all()?;
    std::fs::rename(temp, path)
}

pub fn read_state(path: &Path) -> io::Result<Option<State>> {
    let bytes = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
    {
        Ok(file) => {
            use std::io::Read;
            let mut bytes = Vec::new();
            let mut file = file;
            file.read_to_end(&mut bytes)?;
            bytes
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(io::Error::other)
}

pub fn start_ticks(pid: u32) -> Option<u64> {
    process_start_ticks(pid).ok()
}

pub fn process_start_ticks(pid: u32) -> io::Result<u64> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/stat"))?;
    let after_name = text
        .rsplit_once(')')
        .map(|(_, rest)| rest)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid process stat"))?;
    after_name
        .split_whitespace()
        .nth(19)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing process start time"))?
        .parse()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid process start time"))
}

pub fn pid_alive_with_ticks(pid: u32, ticks: u64) -> bool {
    start_ticks(pid) == Some(ticks)
}

pub fn remove_if_stale(state_path: &Path, sock_path: &Path) -> io::Result<bool> {
    let Some(state) = read_state(state_path)? else {
        return Ok(false);
    };
    let stale = match (state.owner_pid, state.owner_start_ticks) {
        (Some(pid), Some(ticks)) => !pid_alive_with_ticks(pid, ticks),
        (Some(_), None) => false,
        (None, _) => false,
    };
    if !stale {
        return Ok(false);
    }
    not_found_ok(std::fs::remove_file(state_path))?;
    not_found_ok(std::fs::remove_file(sock_path))?;
    Ok(true)
}

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

pub async fn serve_socket<F, Fut>(path: impl AsRef<Path>, handler: F) -> io::Result<SocketHandle>
where
    F: Fn(Value) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Value> + Send + 'static,
{
    let path = path.as_ref().to_path_buf();
    not_found_ok(std::fs::remove_file(&path))?;
    let listener = UnixListener::bind(&path)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    let handler = Arc::new(handler);
    let permits = Arc::new(Semaphore::new(SOCKET_CLIENT_CAP));
    let task = tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let Ok(permit) = Arc::clone(&permits).try_acquire_owned() else {
                continue;
            };
            let handler = Arc::clone(&handler);
            tokio::spawn(handle_client(stream, handler, permit));
        }
    });
    Ok(SocketHandle { path, task })
}

pub fn unknown_command() -> Value {
    serde_json::json!({"error": "unknown cmd"})
}

async fn handle_client<F, Fut>(
    stream: UnixStream,
    handler: Arc<F>,
    _permit: tokio::sync::OwnedSemaphorePermit,
) where
    F: Fn(Value) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Value> + Send + 'static,
{
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read);
    loop {
        let line = match tokio::time::timeout(SOCKET_TIMEOUT, read_line_limited(&mut reader)).await
        {
            Ok(Ok(Some(line))) => line,
            _ => break,
        };
        let response = match serde_json::from_slice::<Value>(&line) {
            Ok(request) => {
                let known = matches!(
                    request.get("cmd").and_then(Value::as_str),
                    Some("status") | Some("handoff")
                );
                if known {
                    match tokio::time::timeout(SOCKET_TIMEOUT, handler(request)).await {
                        Ok(response) => response,
                        Err(_) => serde_json::json!({"error": "request timed out"}),
                    }
                } else {
                    unknown_command()
                }
            }
            Err(_) => serde_json::json!({"error": "invalid json"}),
        };
        let Ok(mut bytes) = serde_json::to_vec(&response) else {
            break;
        };
        bytes.push(b'\n');
        if !matches!(
            tokio::time::timeout(SOCKET_TIMEOUT, write.write_all(&bytes)).await,
            Ok(Ok(()))
        ) {
            break;
        }
    }
}

async fn read_line_limited<R: AsyncBufRead + Unpin>(reader: &mut R) -> io::Result<Option<Vec<u8>>> {
    let mut line = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return if line.is_empty() {
                Ok(None)
            } else {
                Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "socket line has no newline",
                ))
            };
        }
        let take = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |index| index + 1);
        let has_newline = available[..take].contains(&b'\n');
        if line.len() + take > SOCKET_LINE_CAP {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "socket request line is too long",
            ));
        }
        line.extend_from_slice(&available[..take]);
        reader.consume(take);
        if has_newline {
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            return Ok(Some(line));
        }
    }
}

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
        let mut reader = BufReader::new(read);
        let line = read_line_limited(&mut reader)
            .await?
            .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "socket closed"))?;
        serde_json::from_slice(&line).map_err(io::Error::other)
    })
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "socket request timed out"))?
}

#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};

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
        assert!(remove_if_stale(&project_files.state, &project_files.sock).unwrap());
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
        assert!(remove_if_stale(&project_files.state, &project_files.sock).unwrap());
    }

    #[test]
    fn foreign_project_state_is_never_reused() {
        let mut current = state(None, None);
        current.mode = Mode::Gui;
        let pid = std::process::id();
        current.godot_pid = Some(pid);
        current.godot_start_ticks = start_ticks(pid);
        assert!(gui_process_alive(&current));
        assert!(matches_project(&current, Path::new("/project")));
        assert!(!matches_project(&current, Path::new("/other")));
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
