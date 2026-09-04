use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::TempDir;
use url::Url;

struct Client {
    child: Child,
    stdin: Option<ChildStdin>,
    messages: Receiver<Value>,
}

impl Client {
    fn start(project: &Path, runtime: &Path) -> Self {
        let config = runtime.join("config");
        std::fs::create_dir_all(&config).unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_godot-bridge"))
            .arg("lsp")
            .current_dir(project)
            .env("XDG_RUNTIME_DIR", runtime)
            .env("XDG_CONFIG_HOME", &config)
            .env("GODOT_BRIDGE_LOG", "error")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
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

    fn send(&mut self, message: Value) {
        let body = serde_json::to_vec(&message).unwrap();
        let stdin = self.stdin.as_mut().unwrap();
        write!(stdin, "Content-Length: {}\r\n\r\n", body.len()).unwrap();
        stdin.write_all(&body).unwrap();
        stdin.flush().unwrap();
    }

    fn receive_until(&self, timeout: Duration, predicate: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.checked_duration_since(Instant::now()).unwrap();
            let message = self.messages.recv_timeout(remaining).unwrap();
            if predicate(&message) {
                return message;
            }
        }
    }

    fn close_stdin(&mut self) {
        self.stdin.take();
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.stdin.take();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn godot_available() -> bool {
    if Path::new("/usr/bin/godot").is_file() {
        true
    } else {
        println!("skipping LSP integration test: /usr/bin/godot is missing");
        false
    }
}

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../fixtures")
        .join(name)
}

fn file_uri(path: &Path) -> String {
    Url::from_file_path(path.canonicalize().unwrap())
        .unwrap()
        .to_string()
}

fn initialize(client: &mut Client, project: &Path) -> Value {
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

fn runtime_state(runtime: &Path, project: &Path) -> (PathBuf, Value) {
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

fn close_and_wait(client: &mut Client, runtime: &Path, project: &Path) {
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
    assert!(!runtime
        .join("godot-bridge")
        .join(format!("{}.lock", &hash[..16]))
        .exists());
    assert!(!runtime
        .join("godot-bridge")
        .join(format!("{}.sock", &hash[..16]))
        .exists());
}

#[test]
fn minimal_project_diagnostics_and_cleanup() {
    if !godot_available() {
        return;
    }
    let runtime = TempDir::new().unwrap();
    let project = fixture("minimal-project");
    let mut client = Client::start(&project, runtime.path());
    assert!(initialize(&mut client, &project).get("error").is_none());
    client.send(json!({
        "jsonrpc": "2.0",
        "method": "textDocument/didOpen",
        "params": {"textDocument": {"uri": file_uri(&project.join("other.gd")), "languageId": "gdscript", "version": 9, "text": std::fs::read_to_string(project.join("other.gd")).unwrap()}}
    }));
    let diagnostics = client.receive_until(Duration::from_secs(60), |message| {
        message["method"] == "textDocument/publishDiagnostics"
    });
    assert!(!diagnostics["params"]["diagnostics"]
        .as_array()
        .unwrap()
        .is_empty());
    close_and_wait(&mut client, runtime.path(), &project);
}

#[test]
fn nested_project_is_selected() {
    if !godot_available() {
        return;
    }
    let runtime = TempDir::new().unwrap();
    let project = fixture("nested");
    let expected = fixture("nested/repo/game").canonicalize().unwrap();
    let mut client = Client::start(&project, runtime.path());
    assert!(initialize(&mut client, &project).get("error").is_none());
    let (_, state) = runtime_state(runtime.path(), &expected);
    assert_eq!(state["project"], expected.to_string_lossy().as_ref());
    close_and_wait(&mut client, runtime.path(), &expected);
}

#[test]
fn second_owner_is_rejected() {
    if !godot_available() {
        return;
    }
    let runtime = TempDir::new().unwrap();
    let project = fixture("minimal-project");
    let mut first = Client::start(&project, runtime.path());
    assert!(initialize(&mut first, &project).get("error").is_none());
    let mut second = Client::start(&project, runtime.path());
    let response = initialize(&mut second, &project);
    assert_eq!(response["error"]["code"], -32002);
    assert!(response["error"]["message"]
        .as_str()
        .unwrap()
        .contains("Another Zed window"));
    close_and_wait(&mut first, runtime.path(), &project);
}

#[test]
fn godot_crash_recovers_completion() {
    if !godot_available() {
        return;
    }
    let runtime = TempDir::new().unwrap();
    let project = fixture("minimal-project");
    let mut client = Client::start(&project, runtime.path());
    assert!(initialize(&mut client, &project).get("error").is_none());
    client.send(json!({"jsonrpc":"2.0","method":"initialized","params":{}}));
    let (_, state) = runtime_state(runtime.path(), &project);
    let pid = state["godot_pid"].as_i64().unwrap();
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGKILL);
    }
    let message = client.receive_until(Duration::from_secs(60), |message| {
        message["method"] == "window/showMessage"
    });
    assert_eq!(message["params"]["type"], 1);
    client.send(json!({
        "jsonrpc":"2.0",
        "id": 2,
        "method":"textDocument/completion",
        "params":{"textDocument":{"uri":file_uri(&project.join("main.gd"))},"position":{"line":4,"character":8}}
    }));
    let response = client.receive_until(Duration::from_secs(60), |message| {
        message.get("id") == Some(&json!(2))
    });
    assert!(response.get("error").is_none(), "{response}");
    close_and_wait(&mut client, runtime.path(), &project);
}
