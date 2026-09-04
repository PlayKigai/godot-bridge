use super::*;

pub(super) async fn run_session(mut session: Session, unmanaged: bool) -> Result<ExitCode> {
    if unmanaged {
        let init = session.proxy.initialize.clone();
        let project = session.proxy.project.clone();
        if let Err(message) = forward_initialize(
            &mut session.editor,
            &mut session.output,
            &mut session.proxy,
            &project,
            session.settings.project_diagnostics,
        )
        .await
        {
            send_error(
                &mut session.output,
                init.get("id").unwrap_or(&Value::Null),
                -32002,
                &message,
            )
            .await?;
            return Ok(ExitCode::from(1));
        }
        session.proxy.initialized_forwarded = true;
    }
    let mut handoff_receiver = session.runtime.handoff_receiver.take();
    if session.settings.project_diagnostics {
        session.watch.watcher = Some(
            docs_state::watch_project(&session.proxy.project).context("cannot watch project")?,
        );
    }
    let mut gui_check = tokio::time::interval(Duration::from_millis(200));
    let mut symbol_tick = tokio::time::interval(Duration::from_millis(50));
    loop {
        let recovered = tokio::select! {
            client = session.input.read_frame() => {
                match client {
                    Ok(Some(body)) => {
                        let message = match parse_message(&body) {
                            Ok(message) => message,
                            Err(error) => {
                                crate::error!("malformed client message: {error}");
                                return exit_session(&mut session, 1).await;
                            }
                        };
                        match message.get("method").and_then(Value::as_str) {
                            Some("exit") => return exit_session(&mut session, 0).await,
                            Some("shutdown") => return shutdown_session(&mut session, message).await,
                            _ => {}
                        }
                        if let Err(error) = forward_client_message(
                            &mut session.editor,
                            &mut session.output,
                            &mut session.proxy,
                            &session.settings,
                            message,
                        ).await {
                            crate::error!("cannot forward client message: {error}");
                            return exit_session(&mut session, 1).await;
                        }
                        Recovered::Continue
                    }
                    Ok(None) => return exit_session(&mut session, 0).await,
                    Err(error) => {
                        crate::error!("malformed client frame: {error}");
                        return exit_session(&mut session, 1).await;
                    }
                }
            }
            server = session.editor.connection.reader.read_frame() => {
                match server {
                    Ok(Some(body)) => {
                        let forwarded = forward_server_message(
                            &mut session.editor,
                            &mut session.output,
                            &mut session.proxy,
                            &body,
                            false,
                        ).await;
                        match forwarded {
                            Ok(()) => Recovered::Continue,
                            Err(error) => {
                                crate::error!("cannot forward server message: {error}");
                                if unmanaged {
                                    return exit_session(&mut session, 1).await;
                                }
                                match recover(&mut session, &error.to_string(), true).await {
                                    Ok(recovered) => recovered,
                                    Err(_) => return exit_session(&mut session, 1).await,
                                }
                            }
                        }
                    }
                    Ok(None) | Err(_) => {
                        if unmanaged {
                            return exit_session(&mut session, 1).await;
                        }
                        let reason = session
                            .editor
                            .child
                            .as_mut()
                            .and_then(|child| child.child.try_wait().ok().flatten());
                        match recover_with_status(&mut session, reason).await {
                            Ok(recovered) => recovered,
                            Err(_) => return exit_session(&mut session, 1).await,
                        }
                    }
                }
            }
            handoff = next_handoff(&mut handoff_receiver), if handoff_receiver.is_some() => {
                match handoff {
                    Some(handoff) => match perform_handoff(&mut session, handoff).await {
                        Ok(recovered) => recovered,
                        Err(_) => return exit_session(&mut session, 1).await,
                    },
                    None => {
                        handoff_receiver = None;
                        Recovered::Continue
                    }
                }
            }
            _ = gui_check.tick(), if session.runtime.mode == Mode::Gui => {
                if gui_process_is_alive(&session.runtime).await {
                    Recovered::Continue
                } else {
                    match recover(&mut session, "GUI exited", false).await {
                        Ok(recovered) => recovered,
                        Err(_) => return exit_session(&mut session, 1).await,
                    }
                }
            }
            watcher_result = next_watcher_event(&mut session.watch.watcher),
                if session.watch.watcher.is_some() =>
            {
                absorb_watcher_event(&mut session.watch, watcher_result);
                Recovered::Continue
            }
            _ = wait_for_watcher_debounce(session.watch.deadline),
                if session.watch.deadline.is_some() =>
            {
                let changes = coalesce_watcher_changes(std::mem::take(&mut session.watch.pending));
                session.watch.deadline = None;
                process_watcher_changes(
                    &mut session.proxy,
                    &mut session.output,
                    Some(&mut session.editor),
                    &session.settings,
                    changes,
                    false,
                ).await?;
                Recovered::Continue
            }
            event = session.proxy.internal_events.recv() => {
                match event {
                    Some(InternalEvent::Document(event)) => {
                        schedule_symbol_event(&mut session.proxy, event);
                        send_due_symbol_requests(&mut session.editor, &mut session.proxy).await?;
                    }
                    Some(InternalEvent::Bulk { generation, document })
                        if generation == session.proxy.bulk_generation =>
                    {
                        process_bulk_document(
                            &mut session.proxy,
                            &mut session.editor,
                            document,
                        ).await?;
                    }
                    Some(InternalEvent::Bulk { .. }) | None => {}
                }
                Recovered::Continue
            }
            _ = symbol_tick.tick() => {
                send_due_symbol_requests(&mut session.editor, &mut session.proxy).await?;
                Recovered::Continue
            }
        };
        if matches!(recovered, Recovered::ClientClosed) {
            return Ok(ExitCode::SUCCESS);
        }
    }
}

async fn exit_session(session: &mut Session, code: u8) -> Result<ExitCode> {
    let child = session.editor.child.take();
    cleanup_runtime(&mut session.runtime, child).await;
    Ok(if code == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(code)
    })
}

async fn shutdown_session(session: &mut Session, message: Value) -> Result<ExitCode> {
    if let Err(error) = forward_client_request(
        &mut session.editor,
        &mut session.output,
        &mut session.proxy,
        message,
    )
    .await
    {
        crate::error!("cannot forward shutdown: {error}");
        return exit_session(session, 1).await;
    }
    match session.editor.connection.reader.read_frame().await {
        Ok(Some(body)) => {
            if let Err(error) = forward_server_message(
                &mut session.editor,
                &mut session.output,
                &mut session.proxy,
                &body,
                true,
            )
            .await
            {
                crate::error!("cannot forward shutdown response: {error}");
                return exit_session(session, 1).await;
            }
        }
        Ok(None) | Err(_) => return exit_session(session, 1).await,
    }
    exit_session(session, 0).await
}

pub(super) async fn forward_initialize(
    editor: &mut Editor,
    output: &mut ClientWriter,
    proxy: &mut ProxyState,
    project: &Path,
    project_diagnostics: bool,
) -> std::result::Result<(), String> {
    let mut initialize = proxy.initialize.clone();
    let original_id = initialize.get("id").cloned().unwrap_or(Value::Null);
    let bridge_id = proxy.next_id;
    proxy.next_id += 1;
    if let Some(params) = initialize.get_mut("params").and_then(Value::as_object_mut) {
        params.remove("initializationOptions");
    }
    initialize["id"] = crate::json!(bridge_id);
    if let Err(error) = send_godot(&mut editor.connection.writer, &initialize, true).await {
        return Err(error.to_string());
    }
    loop {
        let frame = match editor.connection.reader.read_frame().await {
            Ok(Some(frame)) => frame,
            Ok(None) => return Err("Godot closed during initialize".to_owned()),
            Err(error) => return Err(error.to_string()),
        };
        let message = match parse_message(&frame) {
            Ok(message) => message,
            Err(error) => return Err(error),
        };
        if message.get("method").and_then(Value::as_str) == Some("gdscript_client/changeWorkspace")
        {
            if let Err(error) = check_workspace(&message, project, Some(editor.lsp_port)) {
                return Err(error.to_string());
            }
            if message.get("id").is_some()
                && send_godot(
                    &mut editor.connection.writer,
                    &crate::json!({"jsonrpc":"2.0","id":(message["id"].clone()),"result":null}),
                    false,
                )
                .await
                .is_err()
            {
                return Err("cannot answer changeWorkspace".to_owned());
            }
            continue;
        }
        if message.get("id") == Some(&crate::json!(bridge_id)) {
            let mut response = message;
            response["id"] = original_id;
            patch_initialize_response(&mut response);
            if let Err(error) = send_client(output, &response).await {
                return Err(error.to_string());
            }
            if !project_diagnostics && !proxy.workspace_symbols_notice_sent {
                if let Err(error) = send_info_message(
                    output,
                    "Project diagnostics are disabled; workspace symbols cover only files open in Zed.",
                )
                .await
                {
                    return Err(error.to_string());
                }
                proxy.workspace_symbols_notice_sent = true;
            }
            proxy.initialized_forwarded = true;
            return Ok(());
        }
        if message.get("method").is_some() && message.get("id").is_some() {
            let id = message["id"].to_string();
            proxy.server_requests.insert(id);
            if send_client(output, &message).await.is_err() {
                return Err("cannot forward Godot request".to_owned());
            }
        } else if send_client(output, &message).await.is_err() {
            return Err("cannot forward Godot notification".to_owned());
        }
    }
}
