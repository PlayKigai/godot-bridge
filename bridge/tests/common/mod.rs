use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

const SOCKET_TIMEOUT: Duration = Duration::from_secs(5);

pub enum Protocol {
    Lsp,
    #[allow(dead_code)]
    Dap,
}

static GODOT_TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

pub fn lock_godot() -> MutexGuard<'static, ()> {
    GODOT_TEST_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub(crate) fn redirect_directories(command: &mut Command, runtime: &Path, config: &Path) {
    command
        .env("XDG_RUNTIME_DIR", runtime)
        .env("LOCALAPPDATA", runtime)
        .env("XDG_CONFIG_HOME", config);
}

pub struct BridgeClient {
    pub child: Child,
    stdin: Option<std::process::ChildStdin>,
    messages: Receiver<Value>,
}

impl BridgeClient {
    pub fn start(
        protocol: Protocol,
        project: &Path,
        runtime: &Path,
        config: Option<&Path>,
        settings: Option<&str>,
    ) -> Self {
        let config = config
            .map(Path::to_path_buf)
            .unwrap_or_else(|| runtime.join("config"));
        std::fs::create_dir_all(&config).unwrap();
        let command = match protocol {
            Protocol::Lsp => "lsp",
            Protocol::Dap => "dap",
        };
        let mut process = Command::new(env!("CARGO_BIN_EXE_godot-bridge"));
        process
            .arg(command)
            .current_dir(project)
            .env("GODOT_BRIDGE_LOG", "debug")
            .env_remove("GODOT_BRIDGE_SETTINGS")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        redirect_directories(&mut process, runtime, &config);
        if let Some(settings) = settings {
            process.env("GODOT_BRIDGE_SETTINGS", settings);
        }
        let mut child = process.spawn().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (sender, messages) = mpsc::channel();
        thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                let mut header = String::new();
                if reader.read_line(&mut header).unwrap_or(0) == 0 {
                    break;
                }
                let length = header
                    .strip_prefix("Content-Length:")
                    .and_then(|length| length.trim().parse::<usize>().ok())
                    .unwrap();
                let mut separator = [0; 2];
                reader.read_exact(&mut separator).unwrap();
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                if sender.send(serde_json::from_slice(&body).unwrap()).is_err() {
                    break;
                }
            }
        });
        Self {
            stdin: child.stdin.take(),
            child,
            messages,
        }
    }

    pub fn send(&mut self, message: Value) {
        let body = serde_json::to_vec(&message).unwrap();
        let stdin = self.stdin.as_mut().unwrap();
        write!(stdin, "Content-Length: {}\r\n\r\n", body.len()).unwrap();
        stdin.write_all(&body).unwrap();
        stdin.flush().unwrap();
    }

    pub fn receive_until(
        &self,
        timeout: Duration,
        mut predicate: impl FnMut(&Value) -> bool,
    ) -> Value {
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.checked_duration_since(Instant::now()).unwrap();
            let message = self.messages.recv_timeout(remaining).unwrap();
            if predicate(&message) {
                return message;
            }
        }
    }

    pub fn close_stdin(&mut self) {
        self.stdin.take();
    }
}

impl Drop for BridgeClient {
    fn drop(&mut self) {
        self.stdin.take();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../fixtures")
        .join(name)
}

/// The path the bridge itself would record, so hashes, URIs and the `project`
/// field of a state file all compare equal on both platforms.
pub fn canonical(path: &Path) -> PathBuf {
    godot_bridge::root::canonicalize(path).unwrap()
}

pub fn file_uri(path: &Path) -> String {
    godot_bridge::file_uri::path_to_uri(&canonical(path))
}

pub fn godot_available(test: &str) -> bool {
    if godot_path().is_some() {
        true
    } else {
        println!("skipping {test}: Godot is missing");
        false
    }
}

/// A GUI test needs a desktop session.
#[allow(dead_code)]
pub fn display_available(test: &str) -> bool {
    if cfg!(windows)
        || std::env::var_os("DISPLAY").is_some()
        || std::env::var_os("WAYLAND_DISPLAY").is_some()
    {
        return true;
    }
    println!("skipping {test}: DISPLAY and WAYLAND_DISPLAY are unset");
    false
}

pub fn godot_path() -> Option<PathBuf> {
    godot_bridge::godot_bin::resolve_godot(None).ok()
}

/// The resolved Godot binary as a JSON string, escaped so a Windows path with
/// backslashes survives being embedded in a settings document.
#[allow(dead_code)]
pub fn godot_path_json() -> String {
    serde_json::to_string(&godot_path().unwrap()).unwrap()
}

pub fn initialize_lsp(client: &mut BridgeClient, project: &Path) -> Value {
    client.send(json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "workspaceFolders": [{"uri": file_uri(project), "name": "fixture"}],
            "initializationOptions": {"godot_path": godot_path().unwrap(), "startup_timeout_s": 60}
        }
    }));
    client.receive_until(Duration::from_secs(60), |message| {
        message.get("id") == Some(&json!(1))
    })
}

#[allow(dead_code)]
pub fn initialize_dap(client: &mut BridgeClient) -> Value {
    client.send(json!({
        "seq": 1,
        "type": "request",
        "command": "initialize",
        "arguments": {"adapterID": "godot", "linesStartAt1": true, "columnsStartAt1": true}
    }));
    client.receive_until(Duration::from_secs(60), |message| {
        message["type"] == "response" && message["command"] == "initialize"
    })
}

fn project_hash(project: &Path) -> String {
    let path = canonical(project);
    let path = path.to_string_lossy();
    format!(
        "{}-{}",
        godot_bridge::fnv::hash_hex(path.as_bytes()),
        path.len()
    )
}

fn state_path(runtime: &Path, project: &Path) -> PathBuf {
    runtime
        .join("godot-bridge")
        .join(format!("{}.json", project_hash(project)))
}

#[allow(dead_code)]
pub fn runtime_state(runtime: &Path, project: &Path) -> (PathBuf, Value) {
    let state = state_path(runtime, project);
    let value: Value = serde_json::from_slice(&std::fs::read(&state).unwrap()).unwrap();
    (state, value)
}

#[allow(dead_code)]
pub fn close_and_wait(client: &mut BridgeClient, runtime: &Path, project: &Path) {
    let (state_path, state) = runtime_state(runtime, project);
    let godot_pid = state["godot_pid"].as_u64().unwrap() as u32;
    client.close_stdin();
    let deadline = Instant::now() + Duration::from_secs(6);
    while Instant::now() < deadline {
        if !process_alive(godot_pid) {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    let _ = client.child.wait();
    assert!(!process_alive(godot_pid));
    assert!(!state_path.exists());
    let hash = project_hash(project);
    assert!(runtime
        .join("godot-bridge")
        .join(format!("{hash}.lock"))
        .exists());
    #[cfg(unix)]
    assert!(!runtime
        .join("godot-bridge")
        .join(format!("{hash}.sock"))
        .exists());
}

#[allow(dead_code)]
pub fn socket_status(runtime: &Path, project: &Path) -> Option<Value> {
    let address = godot_bridge::state::socket_path_for_state(&state_path(runtime, project));
    let project = canonical(project).to_string_lossy().into_owned();
    let request = godot_bridge::json!({"cmd": "status", "project": (project)});
    let response = godot_bridge::state::socket_request(address, &request, SOCKET_TIMEOUT).ok()?;
    serde_json::from_str(&response.to_string()).ok()
}

#[allow(dead_code)]
pub fn wait_for_ready(runtime: &Path, project: &Path) -> Value {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Some(status) = socket_status(runtime, project) {
            if status["status"] == "ready" {
                return status;
            }
        }
        assert!(Instant::now() < deadline, "owner did not become ready");
        thread::sleep(Duration::from_millis(200));
    }
}

#[cfg(unix)]
#[allow(dead_code)]
pub fn child_pids(pid: u32) -> Vec<u32> {
    std::fs::read_to_string(format!("/proc/{pid}/task/{pid}/children"))
        .unwrap_or_default()
        .split_whitespace()
        .filter_map(|child| child.parse().ok())
        .collect()
}

#[cfg(unix)]
#[allow(dead_code)]
pub fn process_alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

#[cfg(unix)]
#[allow(dead_code)]
pub fn kill_process(pid: u32) {
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGKILL);
    }
}

#[cfg(windows)]
#[allow(dead_code)]
pub fn child_pids(pid: u32) -> Vec<u32> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    let Some(snapshot) = owned(unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }) else {
        return Vec::new();
    };
    let mut entry = PROCESSENTRY32W {
        dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    let mut children = Vec::new();
    let mut more = unsafe { Process32FirstW(snapshot.as_raw_handle(), &mut entry) };
    while more != 0 {
        if entry.th32ParentProcessID == pid {
            children.push(entry.th32ProcessID);
        }
        more = unsafe { Process32NextW(snapshot.as_raw_handle(), &mut entry) };
    }
    children
}

/// A pid whose process object still exists but has already exited is not
/// alive, so the exit code decides rather than the mere ability to open it.
#[cfg(windows)]
#[allow(dead_code)]
pub fn process_alive(pid: u32) -> bool {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::STILL_ACTIVE;
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    let Some(process) = owned(unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) })
    else {
        return false;
    };
    let mut code = 0u32;
    let read = unsafe { GetExitCodeProcess(process.as_raw_handle(), &mut code) };
    read != 0 && code == STILL_ACTIVE as u32
}

#[cfg(windows)]
#[allow(dead_code)]
pub fn kill_process(pid: u32) {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};
    if let Some(process) = owned(unsafe { OpenProcess(PROCESS_TERMINATE, 0, pid) }) {
        unsafe { TerminateProcess(process.as_raw_handle(), 1) };
    }
}

#[cfg(windows)]
fn owned(
    handle: windows_sys::Win32::Foundation::HANDLE,
) -> Option<std::os::windows::io::OwnedHandle> {
    use std::os::windows::io::{FromRawHandle, OwnedHandle};
    if handle.is_null() || handle == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
        return None;
    }
    Some(unsafe { OwnedHandle::from_raw_handle(handle) })
}

#[allow(dead_code)]
pub fn copy_directory(source: &Path, destination: &Path) {
    std::fs::create_dir_all(destination).unwrap();
    for entry in std::fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_directory(&source_path, &destination_path);
        } else {
            std::fs::copy(source_path, destination_path).unwrap();
        }
    }
}

#[allow(dead_code)]
pub fn status(runtime: &Path, project: &Path, config: &Path) -> Option<Value> {
    let mut command = Command::new(env!("CARGO_BIN_EXE_godot-bridge"));
    command
        .arg("status")
        .current_dir(project)
        .env_remove("GODOT_BRIDGE_SETTINGS");
    redirect_directories(&mut command, runtime, config);
    let output = command.output().ok()?;
    output
        .stdout
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .find_map(|line| serde_json::from_slice(line).ok())
}

#[allow(dead_code)]
pub fn wait_for_status(
    runtime: &Path,
    project: &Path,
    config: &Path,
    predicate: impl Fn(&Value) -> bool,
) -> Value {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Some(value) = status(runtime, project, config) {
            if predicate(&value) {
                return value;
            }
        }
        assert!(
            Instant::now() < deadline,
            "status did not reach expected state"
        );
        thread::sleep(Duration::from_millis(200));
    }
}
