#![allow(dead_code)]

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};
use url::Url;

pub enum Protocol {
    Lsp,
    Dap,
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
            .env("XDG_RUNTIME_DIR", runtime)
            .env("XDG_CONFIG_HOME", &config)
            .env("GODOT_BRIDGE_LOG", "error")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
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

    pub fn receive_until(&self, timeout: Duration, predicate: impl Fn(&Value) -> bool) -> Value {
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

pub fn file_uri(path: &Path) -> String {
    Url::from_file_path(path.canonicalize().unwrap())
        .unwrap()
        .to_string()
}

pub fn godot_available(test: &str) -> bool {
    if Path::new("/usr/bin/godot").is_file() {
        true
    } else {
        println!("skipping {test}: /usr/bin/godot is missing");
        false
    }
}

pub fn initialize_lsp(client: &mut BridgeClient, project: &Path) -> Value {
    client.send(json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "workspaceFolders": [{"uri": file_uri(project), "name": "fixture"}],
            "initializationOptions": {"godot_path": "/usr/bin/godot", "startup_timeout_s": 60}
        }
    }));
    client.receive_until(Duration::from_secs(60), |message| {
        message.get("id") == Some(&json!(1))
    })
}

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

pub fn runtime_state(runtime: &Path, project: &Path) -> (PathBuf, Value) {
    let canonical = project.canonicalize().unwrap();
    let hash = blake3::hash(canonical.to_string_lossy().as_bytes())
        .to_hex()
        .to_string();
    let state = runtime
        .join("godot-bridge")
        .join(format!("{}.json", &hash[..16]));
    let value: Value = serde_json::from_slice(&std::fs::read(&state).unwrap()).unwrap();
    (state, value)
}

pub fn close_and_wait(client: &mut BridgeClient, runtime: &Path, project: &Path) {
    let (state_path, state) = runtime_state(runtime, project);
    let godot_pid = state["godot_pid"].as_u64().unwrap();
    client.close_stdin();
    let deadline = Instant::now() + Duration::from_secs(6);
    while Instant::now() < deadline {
        if !Path::new(&format!("/proc/{godot_pid}")).exists() {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    let _ = client.child.wait();
    assert!(!Path::new(&format!("/proc/{godot_pid}")).exists());
    assert!(!state_path.exists());
    let hash = blake3::hash(project.canonicalize().unwrap().to_string_lossy().as_bytes())
        .to_hex()
        .to_string();
    assert!(runtime
        .join("godot-bridge")
        .join(format!("{}.lock", &hash[..16]))
        .exists());
    assert!(!runtime
        .join("godot-bridge")
        .join(format!("{}.sock", &hash[..16]))
        .exists());
}

pub fn runtime_socket(runtime: &Path, project: &Path) -> PathBuf {
    let hash = blake3::hash(project.canonicalize().unwrap().to_string_lossy().as_bytes())
        .to_hex()
        .to_string();
    runtime
        .join("godot-bridge")
        .join(format!("{}.sock", &hash[..16]))
}

pub fn socket_status(runtime: &Path, project: &Path) -> Option<Value> {
    let mut stream = UnixStream::connect(runtime_socket(runtime, project)).ok()?;
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream.write_all(b"{\"cmd\":\"status\"}\n").ok()?;
    stream.shutdown(Shutdown::Write).ok()?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).ok()?;
    serde_json::from_str(&line).ok()
}

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

pub fn child_pids(pid: u64) -> Vec<u32> {
    std::fs::read_to_string(format!("/proc/{pid}/task/{pid}/children"))
        .unwrap_or_default()
        .split_whitespace()
        .filter_map(|child| child.parse().ok())
        .collect()
}

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

pub fn status(runtime: &Path, project: &Path, config: &Path) -> Option<Value> {
    let output = Command::new(env!("CARGO_BIN_EXE_godot-bridge"))
        .arg("status")
        .current_dir(project)
        .env("XDG_RUNTIME_DIR", runtime)
        .env("XDG_CONFIG_HOME", config)
        .output()
        .ok()?;
    output
        .stdout
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .find_map(|line| serde_json::from_slice(line).ok())
}

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
