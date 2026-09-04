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
    fn start(project: &Path, runtime: &Path, config: &Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_godot-bridge"))
            .arg("lsp")
            .current_dir(project)
            .env("XDG_RUNTIME_DIR", runtime)
            .env("XDG_CONFIG_HOME", config)
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
}

impl Drop for Client {
    fn drop(&mut self) {
        self.stdin.take();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../fixtures/minimal-project")
}

fn file_uri(path: &Path) -> String {
    Url::from_file_path(path.canonicalize().unwrap())
        .unwrap()
        .to_string()
}

fn status(runtime: &Path, project: &Path, config: &Path) -> Option<Value> {
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

fn wait_for_status(
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

fn godot_available() -> bool {
    if Path::new("/usr/bin/godot").is_file() {
        true
    } else {
        println!("skipping open-editor integration test: /usr/bin/godot is missing");
        false
    }
}

#[test]
fn open_editor_handoff_and_gui_recovery() {
    if std::env::var_os("DISPLAY").is_none() && std::env::var_os("WAYLAND_DISPLAY").is_none() {
        println!("skipping open-editor integration test: DISPLAY and WAYLAND_DISPLAY are unset");
        return;
    }
    if !godot_available() {
        return;
    }

    let runtime = TempDir::new().unwrap();
    let config = TempDir::new().unwrap();
    let config_dir = config.path().join("zed");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("settings.json"),
        r#"{"lsp":{"godot":{"settings":{"godot_path":"/usr/bin/godot","startup_timeout_s":60}}}}"#,
    )
    .unwrap();
    let project = fixture();
    let mut client = Client::start(&project, runtime.path(), config.path());
    client.send(json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "workspaceFolders": [{"uri": file_uri(&project), "name": "fixture"}],
            "initializationOptions": {"godot_path": "/usr/bin/godot", "startup_timeout_s": 60}
        }
    }));
    let initialize = client.receive_until(Duration::from_secs(60), |message| {
        message.get("id") == Some(&json!(1))
    });
    assert!(initialize.get("error").is_none(), "{initialize}");
    client.send(json!({"jsonrpc":"2.0","method":"initialized","params":{}}));
    client.send(json!({
        "jsonrpc":"2.0",
        "method":"textDocument/didOpen",
        "params":{"textDocument":{"uri":file_uri(&project.join("main.gd")),"languageId":"gdscript","version":1,"text":std::fs::read_to_string(project.join("main.gd")).unwrap()}}
    }));

    let output = Command::new(env!("CARGO_BIN_EXE_godot-bridge"))
        .args(["open-editor", "--file", "fixtures/minimal-project/main.gd"])
        .current_dir(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".."))
        .env("XDG_RUNTIME_DIR", runtime.path())
        .env("XDG_CONFIG_HOME", config.path())
        .env("GODOT_BRIDGE_LOG", "error")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let accepted: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(accepted["accepted"], true);

    let gui = wait_for_status(runtime.path(), &project, config.path(), |value| {
        value["status"] == "ready" && value["mode"] == "gui"
    });
    let gui_pid = gui["godot_pid"].as_i64().unwrap();
    client.send(json!({
        "jsonrpc":"2.0",
        "id":2,
        "method":"textDocument/completion",
        "params":{"textDocument":{"uri":file_uri(&project.join("main.gd"))},"position":{"line":4,"character":8}}
    }));
    let completion = client.receive_until(Duration::from_secs(60), |message| {
        message.get("id") == Some(&json!(2))
    });
    assert!(completion.get("error").is_none(), "{completion}");

    unsafe {
        libc::kill(gui_pid as libc::pid_t, libc::SIGKILL);
    }
    wait_for_status(runtime.path(), &project, config.path(), |value| {
        value["status"] == "ready" && value["mode"] == "headless"
    });
}
