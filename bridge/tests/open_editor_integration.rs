mod common;
use common::*;
use godot_bridge::temp::TempDir;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

struct GuiEditorGuard<'a> {
    runtime: &'a Path,
    project: &'a Path,
    config: &'a Path,
}

impl Drop for GuiEditorGuard<'_> {
    fn drop(&mut self) {
        let Some(value) = status(self.runtime, self.project, self.config) else {
            return;
        };
        if value["mode"] != "gui" {
            return;
        }
        if let Some(pid) = value["godot_pid"].as_u64() {
            kill_process(pid as u32);
        }
    }
}

#[test]
fn open_editor_handoff_and_gui_recovery() {
    if !display_available("open-editor integration test") {
        return;
    }
    if !godot_available("open-editor integration test") {
        return;
    }
    let _godot_lock = lock_godot();

    let runtime = TempDir::new().unwrap();
    let config = TempDir::new().unwrap();
    let config_dir = config.path().join("zed");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("settings.json"),
        format!(
            r#"{{"lsp":{{"godot":{{"settings":{{"godot_path":{},"startup_timeout_s":60}}}}}}}}"#,
            godot_path_json()
        ),
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

    let _gui_editor = GuiEditorGuard {
        runtime: runtime.path(),
        project: &project,
        config: config.path(),
    };
    let mut command = Command::new(env!("CARGO_BIN_EXE_godot-bridge"));
    command
        .args(["open-editor", "--file", "fixtures/minimal-project/main.gd"])
        .current_dir(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".."))
        .env("GODOT_BRIDGE_LOG", "error");
    common::redirect_directories(&mut command, runtime.path(), config.path());
    let output = command.output().unwrap();
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
    let gui_pid = gui["godot_pid"].as_u64().unwrap() as u32;
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

    kill_process(gui_pid);
    wait_for_status(runtime.path(), &project, config.path(), |value| {
        value["status"] == "ready" && value["mode"] == "headless"
    });
}
