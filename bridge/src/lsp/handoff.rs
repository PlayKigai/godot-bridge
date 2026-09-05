use super::*;

pub(super) fn serve_owner_socket(
    files: &ProjectFiles,
    state: Arc<RwLock<State>>,
    handoff_sender: mpsc::Sender<HandoffRequest>,
) -> Result<crate::state::SocketHandle> {
    let dap_path = files.dap_lock.clone();
    Ok(serve_socket(&files.sock, move |request: Value| {
        let handoff_sender = handoff_sender.clone();
        let dap_path = dap_path.clone();
        let cmd = request.get("cmd").and_then(Value::as_str);
        if !matches!(cmd, Some("status") | Some("handoff")) {
            return crate::state::unknown_command();
        }
        let requested = request.get("project").and_then(Value::as_str);
        let state = state
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if requested.is_some_and(|requested| requested != state.project) {
            return if cmd == Some("status") {
                crate::json!({"error": "project mismatch"})
            } else {
                crate::json!({"version": 1, "accepted": false, "reason": "project mismatch"})
            };
        }
        match cmd {
            Some("status") => state.to_value(),
            Some("handoff") => {
                let decision = handoff_decision(&state);
                drop(state);
                match decision {
                    HandoffDecision::Reject(reason) => {
                        crate::json!({"version": 1, "accepted": false, "reason": reason})
                    }
                    HandoffDecision::AlreadyGui => crate::json!({"version": 1, "accepted": true}),
                    HandoffDecision::Swap => match try_lock(&dap_path) {
                        Ok(Some(guard)) => {
                            let handoff = HandoffRequest {
                                dap_lock: HandoffLock { guard: Some(guard) },
                            };
                            if handoff_sender.send(handoff).is_ok() {
                                crate::json!({"version": 1, "accepted": true})
                            } else {
                                crate::json!({"version": 1, "accepted": false, "reason": "owner is shutting down"})
                            }
                        }
                        Ok(None) => {
                            crate::json!({"version": 1, "accepted": false, "reason": "a debug session is active"})
                        }
                        Err(error) => {
                            crate::json!({"version": 1, "accepted": false, "reason": (error.to_string())})
                        }
                    },
                }
            }
            _ => crate::state::unknown_command(),
        }
    })?)
}
