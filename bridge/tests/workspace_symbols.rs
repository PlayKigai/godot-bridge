mod common;
use common::*;
use godot_bridge::temp::TempDir;
use serde_json::json;
use std::thread;
use std::time::Duration;

#[test]
fn workspace_symbol_finds_ready_function() {
    if !godot_available("workspace symbol integration test") {
        return;
    }
    let _godot_lock = lock_godot();
    let runtime = TempDir::new().unwrap();
    let project = fixture("minimal-project");
    let mut client = BridgeClient::start(Protocol::Lsp, &project, runtime.path(), None, None);
    let initialize = initialize_lsp(&mut client, &project);
    assert!(initialize.get("error").is_none(), "{initialize}");
    assert_eq!(
        initialize["result"]["capabilities"]["workspaceSymbolProvider"],
        true
    );
    client.send(json!({"jsonrpc":"2.0","method":"initialized","params":{}}));
    client.receive_until(Duration::from_secs(60), |message| {
        message["method"] == "textDocument/publishDiagnostics"
    });
    thread::sleep(Duration::from_secs(1));
    client.send(
        json!({"jsonrpc":"2.0","id":2,"method":"workspace/symbol","params":{"query":"ready"}}),
    );
    let response = client.receive_until(Duration::from_secs(60), |message| {
        message.get("id") == Some(&json!(2))
    });
    let symbols = response["result"].as_array().unwrap();
    let symbol = symbols
        .iter()
        .find(|symbol| symbol["name"] == "_ready")
        .unwrap_or_else(|| panic!("{response}"));
    assert_eq!(symbol["kind"], 6);
    let range = &symbol["location"]["range"];
    for position in ["start", "end"] {
        assert!(range[position]["line"].is_u64());
        assert!(range[position]["character"].is_u64());
    }
    assert!(symbol["location"]["uri"]
        .as_str()
        .unwrap()
        .ends_with("/main.gd"));
    assert_eq!(symbol["containerName"], "main.gd");
    close_and_wait(&mut client, runtime.path(), &project);
}
