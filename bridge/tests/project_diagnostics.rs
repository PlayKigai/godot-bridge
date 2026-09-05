mod common;
use common::*;
use godot_bridge::temp::TempDir;
use serde_json::json;
use std::time::Duration;

#[test]
fn project_diagnostics_scan_create_and_remove() {
    if !godot_available("project diagnostics integration test") {
        return;
    }
    let _godot_lock = lock_godot();
    let project = TempDir::new().unwrap();
    copy_directory(&fixture("minimal-project"), project.path());
    let runtime = TempDir::new().unwrap();
    let mut client = BridgeClient::start(Protocol::Lsp, project.path(), runtime.path(), None, None);
    let initialize = initialize_lsp(&mut client, project.path());
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
