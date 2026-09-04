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
        println!("skipping project diagnostics integration test: /usr/bin/godot is missing");
        false
    }
}

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../fixtures/minimal-project")
}

fn copy_directory(source: &Path, destination: &Path) {
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

fn file_uri(path: &Path) -> String {
    Url::from_file_path(path.canonicalize().unwrap())
        .unwrap()
        .to_string()
}

#[test]
fn project_diagnostics_scan_create_and_remove() {
    if !godot_available() {
        return;
    }
    let project = TempDir::new().unwrap();
    copy_directory(&fixture(), project.path());
    let runtime = TempDir::new().unwrap();
    let mut client = Client::start(project.path(), runtime.path());
    client.send(json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "workspaceFolders": [{"uri": file_uri(project.path()), "name": "fixture"}],
            "initializationOptions": {
                "godot_path": "/usr/bin/godot",
                "startup_timeout_s": 60,
                "project_diagnostics": true
            }
        }
    }));
    let initialize = client.receive_until(Duration::from_secs(60), |message| {
        message.get("id") == Some(&json!(1))
    });
    assert!(initialize.get("error").is_none(), "{initialize}");

    client.send(json!({"jsonrpc": "2.0", "method": "initialized", "params": {}}));
    let other_uri = file_uri(&project.path().join("other.gd"));
    let other_diagnostics = client.receive_until(Duration::from_secs(60), |message| {
        message["method"] == "textDocument/publishDiagnostics"
            && message["params"]["uri"] == other_uri
    });
    assert!(!other_diagnostics["params"]["diagnostics"]
        .as_array()
        .unwrap()
        .is_empty());

    let new_file = project.path().join("watcher.gd");
    std::fs::write(&new_file, "extends Node\nvar value: int = \"text\"\n").unwrap();
    let new_uri = file_uri(&new_file);
    let new_diagnostics = client.receive_until(Duration::from_secs(60), |message| {
        message["method"] == "textDocument/publishDiagnostics"
            && message["params"]["uri"] == new_uri
            && message["params"]["diagnostics"]
                .as_array()
                .is_some_and(|diagnostics| !diagnostics.is_empty())
    });
    assert!(!new_diagnostics["params"]["diagnostics"]
        .as_array()
        .unwrap()
        .is_empty());

    std::fs::remove_file(&new_file).unwrap();
    let removal = client.receive_until(Duration::from_secs(60), |message| {
        message["method"] == "textDocument/publishDiagnostics"
            && message["params"]["uri"] == new_uri
            && message["params"]["diagnostics"]
                .as_array()
                .is_some_and(Vec::is_empty)
    });
    assert!(removal["params"]["diagnostics"]
        .as_array()
        .unwrap()
        .is_empty());
}
