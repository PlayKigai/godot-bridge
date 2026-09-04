mod common;
use common::*;
use serde_json::json;
use std::time::Duration;
use tempfile::TempDir;

#[test]
fn minimal_project_diagnostics_and_cleanup() {
    if !godot_available("lsp integration test") {
        return;
    }
    let runtime = TempDir::new().unwrap();
    let project = fixture("minimal-project");
    let mut client = BridgeClient::start(Protocol::Lsp, &project, runtime.path(), None, None);
    assert!(initialize_lsp(&mut client, &project).get("error").is_none());
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
    if !godot_available("lsp integration test") {
        return;
    }
    let runtime = TempDir::new().unwrap();
    let project = fixture("nested");
    let expected = fixture("nested/repo/game").canonicalize().unwrap();
    let mut client = BridgeClient::start(Protocol::Lsp, &project, runtime.path(), None, None);
    assert!(initialize_lsp(&mut client, &project).get("error").is_none());
    let (_, state) = runtime_state(runtime.path(), &expected);
    assert_eq!(state["project"], expected.to_string_lossy().as_ref());
    close_and_wait(&mut client, runtime.path(), &expected);
}

#[test]
fn second_owner_is_rejected() {
    if !godot_available("lsp integration test") {
        return;
    }
    let runtime = TempDir::new().unwrap();
    let project = fixture("minimal-project");
    let mut first = BridgeClient::start(Protocol::Lsp, &project, runtime.path(), None, None);
    assert!(initialize_lsp(&mut first, &project).get("error").is_none());
    let mut second = BridgeClient::start(Protocol::Lsp, &project, runtime.path(), None, None);
    let response = initialize_lsp(&mut second, &project);
    assert_eq!(response["error"]["code"], -32002);
    assert!(response["error"]["message"]
        .as_str()
        .unwrap()
        .contains("Another Zed window"));
    close_and_wait(&mut first, runtime.path(), &project);
}

#[test]
fn godot_crash_recovers_completion() {
    if !godot_available("lsp integration test") {
        return;
    }
    let runtime = TempDir::new().unwrap();
    let project = fixture("minimal-project");
    let mut client = BridgeClient::start(Protocol::Lsp, &project, runtime.path(), None, None);
    assert!(initialize_lsp(&mut client, &project).get("error").is_none());
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
