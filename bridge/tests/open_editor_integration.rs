mod common;
use common::*;
use godot_bridge::temp::TempDir;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

#[test]
fn open_editor_handoff_and_gui_recovery() {
    if std::env::var_os("DISPLAY").is_none() && std::env::var_os("WAYLAND_DISPLAY").is_none() {
        println!("skipping open-editor integration test: DISPLAY and WAYLAND_DISPLAY are unset");
        return;
    }
    if !godot_available("open-editor integration test") {
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
    let project = fixture("minimal-project");
    let mut client = BridgeClient::start(
        Protocol::Lsp,
        &project,
        runtime.path(),
        Some(config.path()),
        None,
    );
    let initialize = initialize_lsp(&mut client, &project);
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
