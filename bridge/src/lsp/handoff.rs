use super::*;

pub(super) async fn serve_owner_socket(
    files: &ProjectFiles,
    state: Arc<RwLock<State>>,
    handoff_sender: UnboundedSender<HandoffRequest>,
) -> Result<crate::state::SocketHandle> {
    let dap_path = files.dap_lock.clone();
    Ok(serve_socket(&files.sock, move |request| {
        let state = Arc::clone(&state);
        let handoff_sender = handoff_sender.clone();
        let dap_path = dap_path.clone();
        async move {
            match request.get("cmd").and_then(Value::as_str) {
                Some("status") => {
                    serde_json::to_value(&*state.read().await).unwrap_or_else(|_| json!({}))
                }
                Some("handoff") => {
                    let decision = {
                        let state = state.read().await;
                        handoff_decision(&state)
                    };
                    match decision {
                        HandoffDecision::Reject(reason) => {
                            json!({"version": 1, "accepted": false, "reason": reason})
                        }
                        HandoffDecision::AlreadyGui => json!({"version": 1, "accepted": true}),
                        HandoffDecision::Swap => match try_lock(&dap_path) {
                            Ok(Some(guard)) => {
                                let handoff = HandoffRequest {
                                    dap_lock: HandoffLock {
                                        guard: Some(guard),
                                    },
                                };
                                if handoff_sender.send(handoff).is_ok() {
                                    json!({"version": 1, "accepted": true})
                                } else {
                                    json!({"version": 1, "accepted": false, "reason": "owner is shutting down"})
                                }
                            }
                            Ok(None) => {
                                json!({"version": 1, "accepted": false, "reason": "a debug session is active"})
                            }
                            Err(error) => {
                                json!({"version": 1, "accepted": false, "reason": error.to_string()})
                            }
                        },
                    }
                }
                _ => crate::state::unknown_command(),
            }
        }
    })
    .await?)
}
