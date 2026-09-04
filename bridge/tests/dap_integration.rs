use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
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
    fn start(command: &str, project: &Path, runtime: &Path, settings: Option<&str>) -> Self {
        let config = runtime.join("config");
        std::fs::create_dir_all(&config).unwrap();
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
                    .and_then(|value| value.trim().parse::<usize>().ok())
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

fn initialize_lsp(client: &mut Client, project: &Path) -> Value {
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

fn initialize_dap(client: &mut Client) -> Value {
    client.send(json!({
        "type": "request",
        "seq": 1,
        "command": "initialize",
        "arguments": {"adapterID": "godot", "linesStartAt1": true, "columnsStartAt1": true}
    }));
    client.receive_until(Duration::from_secs(60), |message| {
        message["type"] == "response" && message["command"] == "initialize"
    })
}

fn runtime_socket(runtime: &Path, project: &Path) -> PathBuf {
    let hash = blake3::hash(project.canonicalize().unwrap().to_string_lossy().as_bytes())
        .to_hex()
        .to_string();
    runtime
        .join("godot-bridge")
        .join(format!("{}.sock", &hash[..16]))
}

fn socket_status(runtime: &Path, project: &Path) -> Option<Value> {
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

fn wait_for_ready(runtime: &Path, project: &Path) -> Value {
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

fn child_pids(pid: u64) -> Vec<u32> {
    std::fs::read_to_string(format!("/proc/{pid}/task/{pid}/children"))
        .unwrap_or_default()
        .split_whitespace()
        .filter_map(|child| child.parse().ok())
        .collect()
}

fn godot_available() -> bool {
    if Path::new("/usr/bin/godot").is_file() {
        true
    } else {
        println!("skipping DAP integration test: /usr/bin/godot is missing");
        false
    }
}

#[test]
fn dap_without_owner_returns_initialize_failure() {
    let runtime = TempDir::new().unwrap();
    let project = fixture("minimal-project");
    let mut client = Client::start(
        "dap",
        &project,
        runtime.path(),
        Some(r#"{"startup_timeout_s":60}"#),
    );
    let response = initialize_dap(&mut client);
    assert_eq!(response["success"], false);
    assert_eq!(
        response["message"],
        format!(
            "No Godot language server runs for {}. Open a .gd file of the project in Zed first.",
            project.canonicalize().unwrap().display()
        )
    );
    assert_eq!(response["request_seq"], 1);
    assert_eq!(response["seq"], 1);
}

#[test]
fn dap_with_owner_launches_and_terminates_game() {
    if std::env::var_os("DISPLAY").is_none() && std::env::var_os("WAYLAND_DISPLAY").is_none() {
        println!("skipping DAP integration test with owner: DISPLAY and WAYLAND_DISPLAY are unset");
        return;
    }
    if !godot_available() {
        return;
    }
    let runtime = TempDir::new().unwrap();
    let project = fixture("minimal-project");
    let mut owner = Client::start("lsp", &project, runtime.path(), None);
    assert!(initialize_lsp(&mut owner, &project).get("error").is_none());
    owner.send(json!({"jsonrpc":"2.0","method":"initialized","params":{}}));
    let status = wait_for_ready(runtime.path(), &project);
    let editor_pid = status["godot_pid"].as_u64().unwrap();
    let existing_children = child_pids(editor_pid);
    let settings = r#"{"godot_path":"/usr/bin/godot","startup_timeout_s":60}"#;
    let mut dap = Client::start("dap", &project, runtime.path(), Some(settings));
    let initialize = initialize_dap(&mut dap);
    assert_eq!(initialize["success"], true, "{initialize}");
    dap.send(json!({
        "type": "request",
        "seq": 2,
        "command": "launch",
        "arguments": {"adapter": "godot", "request": "launch", "scene": "main"}
    }));
    dap.send(json!({"type":"request","seq":3,"command":"configurationDone"}));
    let first_event = dap.receive_until(Duration::from_secs(60), |message| {
        assert!(
            !(message["type"] == "event"
                && (message["event"] == "exited" || message["event"] == "terminated"))
        );
        message["type"] == "event"
            && (message["event"] == "process" || message["event"] == "output")
    });
    let process = if first_event["event"] == "process" {
        first_event
    } else {
        dap.receive_until(Duration::from_secs(60), |message| {
            assert!(
                !(message["type"] == "event"
                    && (message["event"] == "exited" || message["event"] == "terminated"))
            );
            message["type"] == "event" && message["event"] == "process"
        })
    };
    let _ = process;
    let game_deadline = Instant::now() + Duration::from_secs(60);
    let game_pid = loop {
        if let Some(pid) = child_pids(editor_pid)
            .into_iter()
            .find(|pid| !existing_children.contains(pid))
        {
            break pid;
        }
        assert!(
            Instant::now() < game_deadline,
            "Godot did not start a game process"
        );
        thread::sleep(Duration::from_millis(100));
    };
    dap.send(json!({"type":"request","seq":4,"command":"terminate"}));
    let deadline = Instant::now() + Duration::from_secs(60);
    while Path::new(&format!("/proc/{game_pid}")).exists() {
        assert!(Instant::now() < deadline, "game process did not exit");
        thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(wait_for_ready(runtime.path(), &project)["status"], "ready");
    dap.close_stdin();
    let _ = dap.child.wait();
    owner.close_stdin();
}
