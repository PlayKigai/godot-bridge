mod common;
use common::*;
use godot_bridge::temp::TempDir;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

#[test]
fn dap_without_owner_returns_initialize_failure() {
    if !unix_socket_probe("dap without owner") {
        return;
    }
    let runtime = TempDir::new().unwrap();
    let project = fixture("minimal-project");
    let mut client = BridgeClient::start(
        Protocol::Dap,
        &project,
        runtime.path(),
        None,
        Some(r#"{"startup_timeout_s":60}"#),
    );
    let response = initialize_dap(&mut client);
    assert_eq!(response["success"], false);
    assert_eq!(
        response["message"],
        format!(
            "No Godot language server runs for {}. Open a .gd file of the project in your editor first.",
            canonical(&project).display()
        )
    );
    assert_eq!(response["request_seq"], 1);
    assert_eq!(response["seq"], 1);
}

#[test]
fn dap_first_request_must_be_initialize() {
    let runtime = TempDir::new().unwrap();
    let project = fixture("minimal-project");
    let mut client = BridgeClient::start(
        Protocol::Dap,
        &project,
        runtime.path(),
        None,
        Some(r#"{"startup_timeout_s":60}"#),
    );
    client.send(json!({
        "seq": 1,
        "type": "request",
        "command": "launch",
        "arguments": {"scene": "main"}
    }));
    let response = client.receive_until(Duration::from_secs(10), |message| {
        message["type"] == "response" && message["command"] == "launch"
    });
    assert_eq!(response["success"], false);
    assert_eq!(response["message"], "first request must be initialize");
    assert_eq!(response["request_seq"], 1);
    assert_eq!(client.child.wait().unwrap().code(), Some(1));
}

#[test]
fn dap_attach_against_a_headless_owner_is_refused() {
    if !unix_socket_probe("dap attach against a headless owner") {
        return;
    }
    let runtime = TempDir::new().unwrap();
    let project = fixture("minimal-project");
    let _owner = start_fake_owner(runtime.path(), &project);
    let mut client = BridgeClient::start(
        Protocol::Dap,
        &project,
        runtime.path(),
        None,
        Some(r#"{"startup_timeout_s":60}"#),
    );
    let initialize = initialize_dap(&mut client);
    assert_eq!(initialize["success"], true, "{initialize}");
    client.send(json!({
        "type": "request",
        "seq": 2,
        "command": "attach",
        "arguments": {"adapter": "godot", "request": "attach"}
    }));
    let response = client.receive_until(Duration::from_secs(10), |message| {
        message["type"] == "response" && message["command"] == "attach"
    });
    assert_eq!(response["success"], false, "{response}");
    assert_eq!(response["request_seq"], 2);
    assert_eq!(
        response["message"],
        "attach needs a game started from the Godot editor window: run open-editor, then press Play there. Set lsp_port to use your own editor. Games started by run are not attachable; use launch to debug them."
    );
}

#[test]
fn dap_oversized_request_is_refused() {
    let runtime = TempDir::new().unwrap();
    let project = fixture("minimal-project");
    let godot = start_fake_dap();
    let settings = format!(
        r#"{{"lsp_port":1,"dap_port":{},"startup_timeout_s":60}}"#,
        godot.port
    );
    let mut client = BridgeClient::start(
        Protocol::Dap,
        &project,
        runtime.path(),
        None,
        Some(&settings),
    );
    let initialize = initialize_dap(&mut client);
    assert_eq!(initialize["success"], true, "{initialize}");
    client.send(json!({
        "type": "request",
        "seq": 2,
        "command": "evaluate",
        "arguments": {"expression": ("x".repeat(5 * 1024 * 1024))}
    }));
    let response = client.receive_until(Duration::from_secs(30), |message| {
        message["type"] == "response" && message["command"] == "evaluate"
    });
    assert_eq!(response["success"], false, "{response}");
    assert_eq!(response["request_seq"], 2);
    assert_eq!(response["message"], "message too large for Godot");
}

#[test]
fn dap_attach_with_lsp_port_is_forwarded() {
    let runtime = TempDir::new().unwrap();
    let project = fixture("minimal-project");
    let godot = start_fake_dap();
    let settings = format!(
        r#"{{"lsp_port":1,"dap_port":{},"startup_timeout_s":60}}"#,
        godot.port
    );
    let mut client = BridgeClient::start(
        Protocol::Dap,
        &project,
        runtime.path(),
        None,
        Some(&settings),
    );
    let initialize = initialize_dap(&mut client);
    assert_eq!(initialize["success"], true, "{initialize}");
    client.send(json!({
        "type": "request",
        "seq": 2,
        "command": "attach",
        "arguments": {"adapter": "godot", "request": "attach"}
    }));
    let response = client.receive_until(Duration::from_secs(10), |message| {
        message["type"] == "response" && message["command"] == "attach"
    });
    assert_eq!(response["success"], false, "{response}");
    assert_eq!(response["request_seq"], 2);
    assert_eq!(response["message"], "Godot refused this attach");
}

#[test]
fn dap_tolerates_an_owner_that_starts_late() {
    if !unix_socket_probe("dap late owner") {
        return;
    }
    let runtime = TempDir::new().unwrap();
    let project = fixture("minimal-project");
    let mut client = BridgeClient::start(
        Protocol::Dap,
        &project,
        runtime.path(),
        None,
        Some(r#"{"startup_timeout_s":60}"#),
    );
    client.send(json!({
        "seq": 1,
        "type": "request",
        "command": "initialize",
        "arguments": {"adapterID": "godot"}
    }));
    thread::sleep(Duration::from_millis(1500));
    let _owner = start_fake_owner(runtime.path(), &project);
    let initialize = client.receive_until(Duration::from_secs(60), |message| {
        message["type"] == "response" && message["command"] == "initialize"
    });
    assert_eq!(initialize["success"], true, "{initialize}");
}

#[cfg(unix)]
#[test]
fn dap_client_disconnect_during_startup_exits_promptly() {
    if !unix_socket_probe("dap disconnect during startup") {
        return;
    }
    let runtime = TempDir::new().unwrap();
    let project = fixture("minimal-project");
    let mut client = BridgeClient::start(
        Protocol::Dap,
        &project,
        runtime.path(),
        None,
        Some(r#"{"startup_timeout_s":60}"#),
    );
    client.send(json!({
        "seq": 1,
        "type": "request",
        "command": "initialize",
        "arguments": {"adapterID": "godot"}
    }));
    thread::sleep(Duration::from_millis(300));
    client.close_stdin();
    let start = Instant::now();
    let code = client.child.wait().unwrap().code();
    assert_eq!(code, Some(1));
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "startup abort took {:?}",
        start.elapsed()
    );
}

#[cfg(unix)]
#[test]
fn dap_stale_owner_socket_names_the_restart() {
    if !unix_socket_probe("dap stale owner socket") {
        return;
    }
    let runtime = TempDir::new().unwrap();
    let project = fixture("minimal-project");
    let state = project_state_path(runtime.path(), &project);
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .mode(0o700)
        .recursive(true)
        .create(state.parent().unwrap())
        .unwrap();
    let socket = godot_bridge::state::socket_path_for_state(&state);
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    drop(listener);
    assert!(socket.exists());
    let mut client = BridgeClient::start(
        Protocol::Dap,
        &project,
        runtime.path(),
        None,
        Some(r#"{"startup_timeout_s":60}"#),
    );
    let response = initialize_dap(&mut client);
    assert_eq!(response["success"], false);
    assert_eq!(
        response["message"],
        format!(
            "A previous language server for {} did not exit cleanly; restart it (:GodotRestart / Godot: Restart Language Server)",
            canonical(&project).display()
        )
    );
}

#[test]
fn project_dir_resolves_the_configured_project() {
    let runtime = TempDir::new().unwrap();
    let worktree = TempDir::new().unwrap();
    let game = worktree.path().join("game");
    let nested = worktree.path().join("nested");
    std::fs::create_dir_all(&game).unwrap();
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::write(game.join("project.godot"), "").unwrap();
    std::fs::write(nested.join("project.godot"), "").unwrap();
    std::fs::write(nested.join("main.gd"), "").unwrap();
    let settings = r#"{"project_dir":"game"}"#;

    let mut command = Command::new(env!("CARGO_BIN_EXE_godot-bridge"));
    command
        .args(["project-dir", "--file", "nested/main.gd"])
        .current_dir(worktree.path())
        .env("GODOT_BRIDGE_SETTINGS", settings);
    common::redirect_directories(&mut command, runtime.path(), &runtime.path().join("config"));
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        canonical(&game).display().to_string()
    );

    let absolute = nested.join("main.gd");
    let mut command = Command::new(env!("CARGO_BIN_EXE_godot-bridge"));
    command
        .args(["project-dir", "--file"])
        .arg(&absolute)
        .current_dir(worktree.path())
        .env("GODOT_BRIDGE_SETTINGS", settings);
    common::redirect_directories(&mut command, runtime.path(), &runtime.path().join("config"));
    let output = command.output().unwrap();
    assert!(!output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stderr).trim(),
        format!(
            "{} is outside project_dir {}",
            canonical(&absolute).display(),
            canonical(&game).display()
        )
    );
}

#[test]
fn dap_project_dir_wins_over_a_nested_file() {
    if !unix_socket_probe("dap project_dir") {
        return;
    }
    let runtime = TempDir::new().unwrap();
    let worktree = TempDir::new().unwrap();
    let game = worktree.path().join("game");
    let nested = worktree.path().join("nested");
    std::fs::create_dir_all(&game).unwrap();
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::write(game.join("project.godot"), "").unwrap();
    std::fs::write(nested.join("project.godot"), "").unwrap();
    std::fs::write(nested.join("main.gd"), "").unwrap();
    let settings = r#"{"project_dir":"game","startup_timeout_s":600}"#;

    let mut nested_client = BridgeClient::start_dap_with_file(
        worktree.path(),
        runtime.path(),
        settings,
        "nested/main.gd",
    );
    let response = initialize_dap(&mut nested_client);
    assert_eq!(response["success"], false, "{response}");
    assert_eq!(
        response["message"],
        format!(
            "No Godot language server runs for {}. Open a .gd file of the project in your editor first.",
            canonical(&game).display()
        )
    );

    let absolute = nested.join("main.gd");
    let mut absolute_client = BridgeClient::start_dap_with_file(
        worktree.path(),
        runtime.path(),
        settings,
        absolute.to_str().unwrap(),
    );
    let response = initialize_dap(&mut absolute_client);
    assert_eq!(response["success"], false, "{response}");
    assert_eq!(
        response["message"],
        format!(
            "{} is outside project_dir {}",
            canonical(&absolute).display(),
            canonical(&game).display()
        )
    );
}

#[test]
fn dap_with_owner_launches_and_terminates_game() {
    if !unix_socket_probe("DAP integration test with owner") {
        return;
    }
    if !display_available("DAP integration test with owner") {
        return;
    }
    if !godot_available("DAP integration test") {
        return;
    }
    let _godot_lock = lock_godot();
    let runtime = TempDir::new().unwrap();
    let project = fixture("minimal-project");
    let mut owner = BridgeClient::start(Protocol::Lsp, &project, runtime.path(), None, None);
    assert!(initialize_lsp(&mut owner, &project).get("error").is_none());
    owner.send(json!({"jsonrpc":"2.0","method":"initialized","params":{}}));
    let status = wait_for_ready(runtime.path(), &project);
    let editor_pid = status["godot_pid"].as_u64().unwrap() as u32;
    let existing_children = child_pids(editor_pid);
    let settings = format!(
        r#"{{"godot_path":{},"startup_timeout_s":60}}"#,
        godot_path_json()
    );
    let mut dap = BridgeClient::start(
        Protocol::Dap,
        &project,
        runtime.path(),
        None,
        Some(&settings),
    );
    let initialize = initialize_dap(&mut dap);
    assert_eq!(initialize["success"], true, "{initialize}");
    dap.send(json!({
        "type": "request",
        "seq": 2,
        "command": "launch",
        "arguments": {"adapter": "godot", "request": "launch", "scene": "main"}
    }));
    dap.send(json!({"type":"request","seq":3,"command":"configurationDone"}));
    let mut seen_process = false;
    dap.receive_until(Duration::from_secs(60), |message| {
        assert!(
            !(message["type"] == "event"
                && (message["event"] == "exited" || message["event"] == "terminated"))
        );
        if message["type"] == "event" && message["event"] == "process" {
            seen_process = true;
        }
        seen_process
    });
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
    while process_alive(game_pid) {
        assert!(Instant::now() < deadline, "game process did not exit");
        thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(wait_for_ready(runtime.path(), &project)["status"], "ready");
    dap.close_stdin();
    let _ = dap.child.wait();
    owner.close_stdin();
}

#[cfg(unix)]
fn unix_socket_probe(test: &str) -> bool {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("probe.sock");
    match std::os::unix::net::UnixListener::bind(&path) {
        Ok(listener) => {
            drop(listener);
            let _ = std::fs::remove_file(&path);
            true
        }
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            println!("skipping {test}: AF_UNIX sockets are unavailable ({error})");
            false
        }
        Err(_) => true,
    }
}

#[cfg(windows)]
fn unix_socket_probe(_test: &str) -> bool {
    true
}

fn project_state_path(runtime: &Path, project: &Path) -> PathBuf {
    let path = canonical(project);
    let path = path.to_string_lossy();
    let hash = format!(
        "{}-{}",
        godot_bridge::fnv::hash_hex(path.as_bytes()),
        path.len()
    );
    runtime.join("godot-bridge").join(format!("{hash}.json"))
}

/// A Godot debug adapter that answers `initialize` and `attach`.
struct FakeDap {
    stop: Arc<AtomicBool>,
    server: Option<thread::JoinHandle<()>>,
    port: u16,
}

impl Drop for FakeDap {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(server) = self.server.take() {
            let _ = server.join();
        }
    }
}

fn start_fake_dap() -> FakeDap {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let stop = Arc::new(AtomicBool::new(false));
    let server = {
        let stop = Arc::clone(&stop);
        thread::spawn(move || dap_server(listener, &stop))
    };
    FakeDap {
        stop,
        server: Some(server),
        port,
    }
}

/// A language-server owner that answers `status` with `ready` and a `headless`
/// mode, so the DAP path runs without Godot.
struct FakeOwner {
    _dap: FakeDap,
    _socket: godot_bridge::state::SocketHandle,
}

fn start_fake_owner(runtime: &Path, project: &Path) -> FakeOwner {
    let dap = start_fake_dap();
    let dap_port = u64::from(dap.port);
    let state = project_state_path(runtime, project);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .mode(0o700)
            .recursive(true)
            .create(state.parent().unwrap())
            .unwrap();
    }
    let socket_path = godot_bridge::state::socket_path_for_state(&state);
    let project_text = canonical(project).to_string_lossy().into_owned();
    let socket = godot_bridge::state::serve_socket(&socket_path, move |_request| {
        godot_bridge::json!({
            "version": 1,
            "project": (project_text.clone()),
            "status": "ready",
            "mode": "headless",
            "dap_port": dap_port
        })
    })
    .unwrap();
    FakeOwner {
        _dap: dap,
        _socket: socket,
    }
}

fn dap_server(listener: TcpListener, stop: &AtomicBool) {
    listener.set_nonblocking(true).unwrap();
    let mut clients = Vec::new();
    while !stop.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((stream, _)) => clients.push(thread::spawn(move || serve_dap_client(stream))),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(20));
            }
            Err(_) => break,
        }
    }
    for client in clients {
        let _ = client.join();
    }
}

fn serve_dap_client(stream: TcpStream) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut writer = stream;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header).unwrap_or(0) == 0 {
            return;
        }
        let Some(length) = header
            .strip_prefix("Content-Length:")
            .and_then(|length| length.trim().parse::<usize>().ok())
        else {
            return;
        };
        let mut separator = [0u8; 2];
        if reader.read_exact(&mut separator).is_err() {
            return;
        }
        let mut body = vec![0u8; length];
        if reader.read_exact(&mut body).is_err() {
            return;
        }
        let Ok(message) = serde_json::from_slice::<Value>(&body) else {
            continue;
        };
        if message["type"] == "request" && message["command"] == "initialize" {
            let response = json!({
                "seq": 1,
                "type": "response",
                "request_seq": message["seq"],
                "command": "initialize",
                "success": true,
                "body": {}
            });
            let body = serde_json::to_vec(&response).unwrap();
            let _ = write!(writer, "Content-Length: {}\r\n\r\n", body.len());
            let _ = writer.write_all(&body);
            let _ = writer.flush();
        }
        if message["type"] == "request" && message["command"] == "attach" {
            let response = json!({
                "seq": 2,
                "type": "response",
                "request_seq": message["seq"],
                "command": "attach",
                "success": false,
                "message": "Godot refused this attach"
            });
            let body = serde_json::to_vec(&response).unwrap();
            let _ = write!(writer, "Content-Length: {}\r\n\r\n", body.len());
            let _ = writer.write_all(&body);
            let _ = writer.flush();
        }
    }
}
