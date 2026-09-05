use crate::json::{Map, Value};
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

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
        let project_str = project.to_str().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("project path {} is not valid UTF-8", project.display()),
            )
        })?;
        let hash = format!(
            "{}-{}",
            crate::fnv::hash_hex(project_str.as_bytes()),
            project_str.len()
        );
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

#[derive(Clone, Debug, PartialEq, Eq)]
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Starting,
    Ready,
    Recovering,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Headless,
    Gui,
    Unmanaged,
}

impl State {
    pub fn to_value(&self) -> Value {
        let mut object = Map::new();
        object.insert("version".to_owned(), self.version.into());
        object.insert("project".to_owned(), self.project.clone().into());
        object.insert(
            "status".to_owned(),
            match self.status {
                Status::Starting => "starting",
                Status::Ready => "ready",
                Status::Recovering => "recovering",
            }
            .into(),
        );
        object.insert(
            "mode".to_owned(),
            match self.mode {
                Mode::Headless => "headless",
                Mode::Gui => "gui",
                Mode::Unmanaged => "unmanaged",
            }
            .into(),
        );
        object.insert("godot_pid".to_owned(), self.godot_pid.into());
        object.insert("godot_pgid".to_owned(), self.godot_pgid.into());
        object.insert("lsp_port".to_owned(), self.lsp_port.into());
        object.insert("dap_port".to_owned(), self.dap_port.into());
        object.insert("owner_pid".to_owned(), self.owner_pid.into());
        object.insert(
            "owner_start_ticks".to_owned(),
            self.owner_start_ticks.into(),
        );
        object.insert(
            "godot_start_ticks".to_owned(),
            self.godot_start_ticks.into(),
        );
        object.insert("started_at".to_owned(), self.started_at.clone().into());
        object.insert(
            "bridge_version".to_owned(),
            self.bridge_version.clone().into(),
        );
        Value::Object(object)
    }

    pub fn from_value(value: &Value) -> Result<Self, String> {
        let object = value
            .as_object()
            .ok_or_else(|| "state must be an object".to_owned())?;
        let version = field(object, "version")?
            .as_u64()
            .ok_or_else(|| "state.version must be an unsigned integer".to_owned())?
            .try_into()
            .map_err(|_| "state.version must be a 32-bit integer".to_owned())?;
        let status = match field(object, "status")?.as_str() {
            Some("starting") => Status::Starting,
            Some("ready") => Status::Ready,
            Some("recovering") => Status::Recovering,
            _ => return Err("state.status is invalid".to_owned()),
        };
        let mode = match field(object, "mode")?.as_str() {
            Some("headless") => Mode::Headless,
            Some("gui") => Mode::Gui,
            Some("unmanaged") => Mode::Unmanaged,
            _ => return Err("state.mode is invalid".to_owned()),
        };
        Ok(Self {
            version,
            project: string_field(object, "project")?,
            status,
            mode,
            godot_pid: optional_u32(object, "godot_pid")?,
            godot_pgid: optional_u32(object, "godot_pgid")?,
            lsp_port: optional_u16(object, "lsp_port")?,
            dap_port: optional_u16(object, "dap_port")?,
            owner_pid: optional_u32(object, "owner_pid")?,
            owner_start_ticks: optional_u64(object, "owner_start_ticks")?,
            godot_start_ticks: optional_u64(object, "godot_start_ticks")?,
            started_at: string_field(object, "started_at")?,
            bridge_version: string_field(object, "bridge_version")?,
        })
    }
}

fn field<'a>(object: &'a Map, key: &str) -> Result<&'a Value, String> {
    object
        .get(key)
        .ok_or_else(|| format!("state is missing {key}"))
}

fn string_field(object: &Map, key: &str) -> Result<String, String> {
    field(object, key)?
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| format!("state.{key} must be a string"))
}

fn optional_u64(object: &Map, key: &str) -> Result<Option<u64>, String> {
    match field(object, key)? {
        Value::Null => Ok(None),
        value => value
            .as_u64()
            .map(Some)
            .ok_or_else(|| format!("state.{key} must be an unsigned integer or null")),
    }
}

fn optional_u32(object: &Map, key: &str) -> Result<Option<u32>, String> {
    optional_u64(object, key)?
        .map(u32::try_from)
        .transpose()
        .map_err(|_| format!("state.{key} must be a 32-bit integer or null"))
}

fn optional_u16(object: &Map, key: &str) -> Result<Option<u16>, String> {
    optional_u64(object, key)?
        .map(u16::try_from)
        .transpose()
        .map_err(|_| format!("state.{key} must be a 16-bit integer or null"))
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
    let bytes = crate::json::to_vec(&state.to_value());
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
    let value = crate::json::from_slice(&bytes).map_err(io::Error::other)?;
    State::from_value(&value)
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
        let _ = std::fs::remove_file(&self.path);
    }
}

pub fn serve_socket<F>(path: impl AsRef<Path>, handler: F) -> io::Result<SocketHandle>
where
    F: Fn(Value) -> Value + Send + Sync + 'static,
{
    let path = path.as_ref().to_path_buf();
    let temporary = path.with_extension("tmp");
    not_found_ok(std::fs::remove_file(&temporary))?;
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

pub fn unknown_command() -> Value {
    crate::json!({"error": "unknown cmd"})
}

fn handle_client<F>(stream: UnixStream, handler: Arc<F>, stop: Arc<AtomicBool>)
where
    F: Fn(Value) -> Value + Send + Sync + 'static,
{
    let _ = stream.set_read_timeout(Some(Duration::from_millis(100)));
    let _ = stream.set_write_timeout(Some(SOCKET_TIMEOUT));
    let reader_stream = match stream.try_clone() {
        Ok(reader_stream) => reader_stream,
        Err(_) => return,
    };
    let mut reader = BufReader::new(reader_stream);
    let mut write = stream;
    loop {
        if stop.load(Ordering::Acquire) {
            break;
        }
        let line = match read_line_limited(&mut reader) {
            Ok(Some(line)) => line,
            Ok(None) => break,
            Err(error) if error.kind() == io::ErrorKind::TimedOut => continue,
            Err(_) => break,
        };
        let response = match crate::json::from_slice(&line) {
            Ok(request) => {
                let known = matches!(
                    request.get("cmd").and_then(Value::as_str),
                    Some("status") | Some("handoff")
                );
                if known {
                    handler(request)
                } else {
                    unknown_command()
                }
            }
            Err(_) => crate::json!({"error": "invalid json"}),
        };
        let mut bytes = crate::json::to_vec(&response);
        bytes.push(b'\n');
        if write.write_all(&bytes).is_err() {
            break;
        }
    }
}

fn read_line_limited<R: BufRead>(reader: &mut R) -> io::Result<Option<Vec<u8>>> {
    let mut line = Vec::new();
    loop {
        let available = reader.fill_buf()?;
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

pub fn socket_request(path: impl AsRef<Path>, req: &Value, timeout: Duration) -> io::Result<Value> {
    let stream = UnixStream::connect(path)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    let mut write = stream.try_clone()?;
    let mut reader = BufReader::new(stream);
    let mut bytes = crate::json::to_vec(req);
    bytes.push(b'\n');
    write.write_all(&bytes)?;
    let line = read_line_limited(&mut reader)?
        .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "socket closed"))?;
    crate::json::from_slice(&line).map_err(io::Error::other)
}

#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::temp::tempdir;
    use std::os::unix::fs::PermissionsExt;

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
    fn project_path_with_invalid_utf8_is_rejected() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        let path = PathBuf::from(OsStr::from_bytes(&[0x66, 0xff]));
        assert!(ProjectFiles::new(&path).is_err());
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
    fn state_naming_another_project_does_not_match() {
        let current = state(None, None);
        assert!(matches_project(&current, Path::new("/project")));
        assert!(!matches_project(&current, Path::new("/other")));
    }

    #[test]
    fn starting_state_serializes_nullable_fields_as_null() {
        let value = state(None, None).to_value();
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
        let value = detached_gui_state(Path::new("/project"), Status::Starting).to_value();
        assert_eq!(value["mode"], "gui");
        assert_eq!(value["status"], "starting");
        assert!(value["owner_pid"].is_null());
        assert!(value["owner_start_ticks"].is_null());
        assert!(value["godot_pid"].is_null());
        assert!(value["lsp_port"].is_null());
        assert!(value["dap_port"].is_null());
    }

    #[test]
    fn socket_status_round_trip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("status.sock");
        let handle = serve_socket(&path, |request| {
            if request["cmd"] == "status" {
                crate::json!({"status": "ready"})
            } else {
                crate::json!({"error": "unexpected"})
            }
        })
        .unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let response = socket_request(
            &path,
            &crate::json!({"cmd": "status"}),
            Duration::from_secs(1),
        )
        .unwrap();
        assert_eq!(response["status"], "ready");
        drop(handle);
        assert!(!path.exists());
    }
}
