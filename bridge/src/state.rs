use crate::json::{Map, Value};
use crate::sys::{self, OpenMode};
use std::io::{self, BufRead};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::sys::process_start_ticks;
pub use crate::sys::{
    fallback_runtime_dir, pid_alive_with_ticks, runtime_dir, LockGuard, SocketHandle,
};

const SOCKET_LINE_CAP: usize = 64 * 1024;
const STATE_FILE_CAP: u64 = 64 * 1024;

/// Take the exclusive lock on `path` without blocking. `Ok(None)` means
/// another process holds it.
pub fn try_lock(path: &Path) -> io::Result<Option<LockGuard>> {
    sys::try_lock(path)
}

/// Answer JSON requests on `path` until the returned handle is dropped.
pub fn serve_socket<F>(path: impl AsRef<Path>, handler: F) -> io::Result<SocketHandle>
where
    F: Fn(Value) -> Value + Send + Sync + 'static,
{
    sys::serve_socket(path.as_ref(), handler)
}

/// Send one JSON request to the owner listening on `path` and read its reply.
pub fn socket_request(path: impl AsRef<Path>, req: &Value, timeout: Duration) -> io::Result<Value> {
    sys::socket_request(path.as_ref(), req, timeout)
}

/// The runtime paths that belong to one project: the owner lock, the request
/// socket, the state file and the debug session lock. On Windows the socket is
/// a named pipe rather than a file under the runtime directory.
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
        let (runtime, sock) = sys::socket_path(&sys::runtime_dir()?, &hash)?;
        let prefix = runtime.join(&hash);
        Ok(Self {
            lock: prefix.with_extension("lock"),
            sock,
            state: prefix.with_extension("json"),
            dap_lock: prefix.with_extension("dap.lock"),
        })
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
        let status = match self.status {
            Status::Starting => "starting",
            Status::Ready => "ready",
            Status::Recovering => "recovering",
        };
        let mode = match self.mode {
            Mode::Headless => "headless",
            Mode::Gui => "gui",
            Mode::Unmanaged => "unmanaged",
        };
        crate::json!({
            "version": (self.version),
            "project": (self.project.clone()),
            "status": status,
            "mode": mode,
            "godot_pid": (self.godot_pid),
            "godot_pgid": (self.godot_pgid),
            "lsp_port": (self.lsp_port),
            "dap_port": (self.dap_port),
            "owner_pid": (self.owner_pid),
            "owner_start_ticks": (self.owner_start_ticks),
            "godot_start_ticks": (self.godot_start_ticks),
            "started_at": (self.started_at.clone()),
            "bridge_version": (self.bridge_version.clone())
        })
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
            godot_pid: optional_int(object, "godot_pid")?,
            godot_pgid: optional_int(object, "godot_pgid")?,
            lsp_port: optional_int(object, "lsp_port")?,
            dap_port: optional_int(object, "dap_port")?,
            owner_pid: optional_int(object, "owner_pid")?,
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

fn optional_int<T: TryFrom<u64>>(object: &Map, key: &str) -> Result<Option<T>, String> {
    optional_u64(object, key)?
        .map(T::try_from)
        .transpose()
        .map_err(|_| {
            format!(
                "state.{key} must be a {}-bit integer or null",
                std::mem::size_of::<T>() * 8
            )
        })
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
    crate::root::paths_equal(Path::new(&state.project), project)
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
    let mut file = sys::open_private(&temp, OpenMode::Truncate)?;
    use std::io::Write;
    file.write_all(&bytes)?;
    file.sync_all()?;
    std::fs::rename(temp, path)
}

pub fn read_state(path: &Path) -> io::Result<Option<State>> {
    let bytes = match sys::open_nofollow_read(path) {
        Ok(file) => {
            use std::io::Read;
            let mut bytes = Vec::new();
            file.take(STATE_FILE_CAP + 1).read_to_end(&mut bytes)?;
            if bytes.len() as u64 > STATE_FILE_CAP {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("state file {} is larger than 64 KiB", path.display()),
                ));
            }
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

/// The rendezvous address that belongs to a state file in the runtime
/// directory, used by `status` to reach an owner it did not start.
pub fn socket_path_for_state(state: &Path) -> PathBuf {
    sys::socket_path_for_state(state)
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
    match std::fs::remove_file(state_path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        other => other?,
    }
    sys::remove_socket(sock_path)?;
    Ok(true)
}

/// Read one newline-terminated request from a socket, refusing a line that
/// would grow past [`SOCKET_LINE_CAP`]. `line` carries the bytes read so far
/// across a `WouldBlock`.
pub(crate) fn read_line_limited<R: BufRead>(
    reader: &mut R,
    line: &mut Vec<u8>,
) -> io::Result<Option<Vec<u8>>> {
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
        let newline = available.iter().position(|byte| *byte == b'\n');
        let take = newline.map_or(available.len(), |index| index + 1);
        let has_newline = newline.is_some();
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
            return Ok(Some(std::mem::take(line)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::temp::TempDir;

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

    #[cfg(unix)]
    #[test]
    fn project_path_with_invalid_utf8_is_rejected() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        let path = PathBuf::from(OsStr::from_bytes(&[0x66, 0xff]));
        assert!(ProjectFiles::new(&path).is_err());
    }

    #[test]
    fn second_lock_is_rejected() {
        let dir = TempDir::new().unwrap();
        let first = try_lock(&dir.path().join("lock")).unwrap().unwrap();
        assert!(try_lock(&dir.path().join("lock")).unwrap().is_none());
        drop(first);
        assert!(try_lock(&dir.path().join("lock")).unwrap().is_some());
    }

    #[test]
    fn runtime_dir_is_a_usable_directory() {
        let dir = runtime_dir().unwrap();
        assert!(dir.is_dir());
        assert_eq!(runtime_dir().unwrap(), dir);
    }

    #[test]
    fn start_ticks_identify_this_process() {
        let pid = std::process::id();
        let ticks = start_ticks(pid).expect("this process has a start time");
        assert_eq!(start_ticks(pid), Some(ticks));
        assert!(pid_alive_with_ticks(pid, ticks));
        assert!(!pid_alive_with_ticks(pid, ticks.wrapping_add(1)));
        assert!(start_ticks(u32::MAX).is_none());
    }

    #[test]
    fn dead_owner_state_is_removed() {
        let dir = TempDir::new().unwrap();
        let project_files = files(dir.path());
        write_state(&project_files.state, &state(Some(u32::MAX), Some(1))).unwrap();
        std::fs::write(&project_files.sock, b"stale").unwrap();
        assert!(remove_if_stale(&project_files.state, &project_files.sock).unwrap());
        assert!(!project_files.state.exists());
        #[cfg(unix)]
        assert!(!project_files.sock.exists());
    }

    #[test]
    fn oversized_state_is_rejected() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("oversized.json");
        let mut bytes = crate::json::to_vec(&state(None, None).to_value());
        bytes.resize(STATE_FILE_CAP as usize + 1, b' ');
        std::fs::write(&path, &bytes).unwrap();
        let error = read_state(&path).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("larger than 64 KiB"), "{error}");

        bytes.truncate(STATE_FILE_CAP as usize);
        std::fs::write(&path, &bytes).unwrap();
        assert_eq!(read_state(&path).unwrap(), Some(state(None, None)));
    }

    #[test]
    fn reused_pid_is_ignored_by_ticks() {
        let dir = TempDir::new().unwrap();
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

    #[cfg(windows)]
    #[test]
    fn project_files_use_a_named_pipe() {
        let files = ProjectFiles::new(Path::new(r"C:\projects\game")).unwrap();
        let sock = files.sock.to_string_lossy().into_owned();
        assert!(sock.starts_with(r"\\.\pipe\godot-bridge-"), "{sock}");
        assert_eq!(socket_path_for_state(&files.state), files.sock);
        assert_eq!(files.lock.extension().unwrap(), "lock");
        assert_eq!(files.state.extension().unwrap(), "json");
    }

    #[cfg(unix)]
    #[test]
    fn socket_status_round_trip() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new().unwrap();
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
