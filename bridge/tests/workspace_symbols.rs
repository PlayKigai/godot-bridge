use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::Duration;
use tempfile::TempDir;
use url::Url;

struct Client {
    child: Child,
    stdin: Option<std::process::ChildStdin>,
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
                    .unwrap()
                    .trim()
                    .parse::<usize>()
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

    fn receive_until(&self, predicate: impl Fn(&Value) -> bool) -> Value {
        loop {
            let message = self.messages.recv_timeout(Duration::from_secs(60)).unwrap();
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

fn file_uri(path: &Path) -> String {
    Url::from_file_path(path.canonicalize().unwrap())
        .unwrap()
        .to_string()
}

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../fixtures/minimal-project")
}

#[test]
fn workspace_symbol_finds_ready_function() {
    if !Path::new("/usr/bin/godot").is_file() {
        println!("skipping workspace symbol integration test: /usr/bin/godot is missing");
        return;
    }
    let runtime = TempDir::new().unwrap();
    let project = fixture();
    let mut client = Client::start(&project, runtime.path());
    client.send(json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"workspaceFolders":[{"uri":file_uri(&project),"name":"fixture"}],"initializationOptions":{"godot_path":"/usr/bin/godot","startup_timeout_s":60}}}));
    let initialize = client.receive_until(|message| message.get("id") == Some(&json!(1)));
    assert!(initialize.get("error").is_none(), "{initialize}");
    assert_eq!(
        initialize["result"]["capabilities"]["workspaceSymbolProvider"],
        true
    );
    client.send(json!({"jsonrpc":"2.0","method":"initialized","params":{}}));
    client.receive_until(|message| message["method"] == "textDocument/publishDiagnostics");
    thread::sleep(Duration::from_secs(1));
    client.send(
        json!({"jsonrpc":"2.0","id":2,"method":"workspace/symbol","params":{"query":"ready"}}),
    );
    let response = client.receive_until(|message| message.get("id") == Some(&json!(2)));
    let symbols = response["result"].as_array().unwrap();
    assert!(
        symbols.iter().any(|symbol| symbol["name"] == "_ready"
            && symbol["location"]["uri"]
                .as_str()
                .unwrap()
                .ends_with("/main.gd")),
        "{response}"
    );
}
