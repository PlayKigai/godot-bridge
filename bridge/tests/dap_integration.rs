mod common;
use common::*;
use godot_bridge::temp::TempDir;
use serde_json::json;
use std::thread;
use std::time::{Duration, Instant};

#[test]
fn dap_without_owner_returns_initialize_failure() {
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
            "No Godot language server runs for {}. Open a .gd file of the project in Zed first.",
            canonical(&project).display()
        )
    );
    assert_eq!(response["request_seq"], 1);
    assert_eq!(response["seq"], 1);
}

#[test]
fn dap_with_owner_launches_and_terminates_game() {
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
