mod common;
use common::*;
use serde_json::json;
use std::thread;
use std::time::Duration;
use tempfile::TempDir;

#[test]
fn workspace_symbol_finds_ready_function() {
    if !godot_available("workspace symbol integration test") {
        return;
    }
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
    assert!(
        symbols.iter().any(|symbol| symbol["name"] == "_ready"
            && symbol["location"]["uri"]
                .as_str()
                .unwrap()
                .ends_with("/main.gd")),
        "{response}"
    );
}
