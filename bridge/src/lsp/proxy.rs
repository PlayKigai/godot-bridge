use super::*;

pub(super) fn run_session(mut session: Session, unmanaged: bool) -> Result<ExitCode> {
    if unmanaged {
        let init = session.proxy.initialize.clone();
        let project = session.proxy.project.clone();
        if let Err(message) = forward_initialize(
            &mut session.editor,
            &mut session.output,
            &mut session.proxy,
            &project,
            session.settings.project_diagnostics,
        ) {
            send_error(
                &mut session.output,
                init.get("id").unwrap_or(&Value::Null),
                -32002,
                &message,
            )?;
            return Ok(ExitCode::from(1));
        }
        session.proxy.initialized_forwarded = true;
    }
    if session.settings.project_diagnostics {
        session.watch.watcher = Some(
            crate::watch::watch_project(&session.proxy.project, session.settings.diagnose_addons)
                .context("cannot watch project")?,
        );
    }
    let gui_interval = Duration::from_millis(200);
    let symbol_interval = Duration::from_millis(50);
    let mut gui_deadline = Instant::now() + gui_interval;
    let mut symbol_deadline = Instant::now() + symbol_interval;
    let mut turn = 0;
    loop {
        let now = Instant::now();
        if session.runtime.mode == Mode::Gui && now >= gui_deadline {
            while gui_deadline <= now {
                gui_deadline += gui_interval;
            }
            if !gui_process_is_alive(&session.runtime) {
                recover(&mut session, "GUI exited", false)?;
            }
            continue;
        }
        if now >= symbol_deadline {
            while symbol_deadline <= now {
                symbol_deadline += symbol_interval;
            }
            if let Err(error) = send_due_symbol_requests(&mut session.editor, &mut session.proxy) {
                if unmanaged {
                    return exit_session(&mut session, 1);
                }
                recover(&mut session, &error.to_string(), true)?;
            }
        }
        if (!session.proxy.bulk_documents.is_empty()
            && (session.proxy.bulk_documents.len() >= BULK_DOCUMENTS
                || session.proxy.bulk_complete))
            && (session
                .proxy
                .bulk_deadline
                .is_some_and(|deadline| deadline <= now)
                || session.proxy.pending.len() < IN_FLIGHT_CAP)
        {
            if let Err(error) =
                pump_bulk_documents(&mut session.proxy, &mut session.editor.connection.writer)
            {
                if unmanaged {
                    return exit_session(&mut session, 1);
                }
                recover(&mut session, &error.to_string(), true)?;
            }
        }
        if session
            .watch
            .deadline
            .is_some_and(|deadline| deadline <= now)
        {
            let changes = coalesce_watcher_changes(std::mem::take(&mut session.watch.pending));
            session.watch.deadline = None;
            if let Err(error) = process_watcher_changes(
                &mut session.proxy,
                &mut session.output,
                Some(&mut session.editor),
                &session.settings,
                changes,
                false,
            ) {
                if unmanaged {
                    return exit_session(&mut session, 1);
                }
                recover(&mut session, &error.to_string(), true)?;
            }
        }
        let mut processed = false;
        for offset in 0..5 {
            match (turn + offset) % 5 {
                0 => {
                    let event = {
                        let input = &mut session.input;
                        let editor = &mut session.editor;
                        let output = &mut session.output;
                        let proxy = &mut session.proxy;
                        let settings = &session.settings;
                        input.try_with_next_frame(|body| {
                            process_client_frame(editor, output, proxy, settings, body)
                        })
                    };
                    match event {
                        Ok(FramePoll::Frame(Ok(frame))) => {
                            processed = true;
                            if let Some(code) = handle_client_frame(&mut session, unmanaged, frame)?
                            {
                                return Ok(code);
                            }
                        }
                        Ok(FramePoll::End) => return exit_session(&mut session, 0),
                        Ok(FramePoll::Empty) => {}
                        Ok(FramePoll::Frame(Err(error))) => {
                            crate::error!("malformed client message: {error}");
                            return exit_session(&mut session, 1);
                        }
                        Err(error) => {
                            crate::error!("malformed client frame: {error}");
                            return exit_session(&mut session, 1);
                        }
                    }
                }
                1 => {
                    let lsp_port = session.editor.lsp_port;
                    let event = {
                        let connection = &mut session.editor.connection;
                        let reader = connection.reader.as_mut();
                        let writer = &mut connection.writer;
                        let output = &mut session.output;
                        let proxy = &mut session.proxy;
                        reader
                            .ok_or_else(|| {
                                crate::framing::FrameError::Io(io::Error::other("reader is closed"))
                            })?
                            .try_with_next_frame(|body| {
                                forward_server_message(writer, lsp_port, output, proxy, body, false)
                            })
                    };
                    match event {
                        Ok(FramePoll::Frame(Ok(()))) => processed = true,
                        Ok(FramePoll::Frame(Err(error))) => {
                            processed = true;
                            crate::error!("cannot forward server message: {error}");
                            if unmanaged {
                                return exit_session(&mut session, 1);
                            }
                            recover(&mut session, &error.to_string(), true)?;
                        }
                        Ok(FramePoll::Empty) => {}
                        Ok(FramePoll::End) | Err(_) => {
                            if unmanaged {
                                return exit_session(&mut session, 1);
                            }
                            let reason = session
                                .editor
                                .child
                                .as_mut()
                                .and_then(|child| child.child.try_wait().ok().flatten());
                            recover_with_status(&mut session, reason)?;
                        }
                    }
                }
                2 => {
                    let handoff = session
                        .runtime
                        .handoff_receiver
                        .as_mut()
                        .and_then(|receiver| receiver.try_recv().ok());
                    if let Some(handoff) = handoff {
                        processed = true;
                        perform_handoff(&mut session, handoff)?;
                    }
                }
                3 => {
                    let watcher_result = session
                        .watch
                        .watcher
                        .as_mut()
                        .and_then(|watcher| watcher.receiver.try_recv().ok());
                    if let Some(watcher_result) = watcher_result {
                        processed = true;
                        absorb_watcher_event(&mut session.watch, Some(watcher_result));
                    }
                }
                _ => match session.proxy.internal_events.try_recv() {
                    Ok(event) => {
                        processed = true;
                        match event {
                            InternalEvent::Document(event) => {
                                schedule_symbol_event(&mut session.proxy, event);
                                if let Err(error) = send_due_symbol_requests(
                                    &mut session.editor,
                                    &mut session.proxy,
                                ) {
                                    if unmanaged {
                                        return exit_session(&mut session, 1);
                                    }
                                    recover(&mut session, &error.to_string(), true)?;
                                }
                            }
                            InternalEvent::Bulk {
                                generation,
                                document,
                            } if generation == session.proxy.bulk_generation => {
                                session.proxy.bulk_documents.push_back(document);
                                if session.proxy.bulk_documents.len() >= BULK_DOCUMENTS {
                                    if let Err(error) = pump_bulk_documents(
                                        &mut session.proxy,
                                        &mut session.editor.connection.writer,
                                    ) {
                                        if unmanaged {
                                            return exit_session(&mut session, 1);
                                        }
                                        recover(&mut session, &error.to_string(), true)?;
                                    }
                                }
                            }
                            InternalEvent::BulkComplete { generation }
                                if generation == session.proxy.bulk_generation =>
                            {
                                session.proxy.bulk_complete = true;
                                if let Err(error) = pump_bulk_documents(
                                    &mut session.proxy,
                                    &mut session.editor.connection.writer,
                                ) {
                                    if unmanaged {
                                        return exit_session(&mut session, 1);
                                    }
                                    recover(&mut session, &error.to_string(), true)?;
                                }
                            }
                            InternalEvent::Bulk { .. } => {}
                            InternalEvent::BulkComplete { .. } => {}
                        }
                    }
                    Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => {}
                },
            }
            if processed {
                break;
            }
        }
        turn = (turn + 1) % 5;
        if processed {
            continue;
        }
        let mut wait = Duration::from_millis(50);
        let now = Instant::now();
        wait = wait.min(
            symbol_deadline
                .checked_duration_since(now)
                .unwrap_or(Duration::ZERO),
        );
        if session.runtime.mode == Mode::Gui {
            wait = wait.min(
                gui_deadline
                    .checked_duration_since(now)
                    .unwrap_or(Duration::ZERO),
            );
        }
        if let Some(deadline) = session.watch.deadline {
            wait = wait.min(
                deadline
                    .checked_duration_since(now)
                    .unwrap_or(Duration::ZERO),
            );
        }
        if wait.is_zero() {
            continue;
        }
        let event = {
            let input = &mut session.input;
            let editor = &mut session.editor;
            let output = &mut session.output;
            let proxy = &mut session.proxy;
            let settings = &session.settings;
            input.recv_timeout_with_frame(wait, |body| {
                process_client_frame(editor, output, proxy, settings, body)
            })
        };
        match event {
            Ok(FramePoll::Frame(Ok(frame))) => {
                if let Some(code) = handle_client_frame(&mut session, unmanaged, frame)? {
                    return Ok(code);
                }
            }
            Ok(FramePoll::End) => return exit_session(&mut session, 0),
            Ok(FramePoll::Empty) => {}
            Ok(FramePoll::Frame(Err(error))) => return Err(error.into()),
            Err(_) => return exit_session(&mut session, 1),
        }
    }
}

enum ClientFrame {
    Exit,
    Shutdown(Value),
    Forward(Result<()>),
}

fn process_client_frame(
    editor: &mut Editor,
    output: &mut ClientWriter,
    proxy: &mut ProxyState,
    settings: &Settings,
    body: &[u8],
) -> std::result::Result<ClientFrame, String> {
    let fields = crate::json::scan_top_level(body).map_err(|error| error.to_string())?;
    if fields.method.is_some_and(|method| method.string_eq("exit")) {
        return Ok(ClientFrame::Exit);
    }
    if fields
        .method
        .is_some_and(|method| method.string_eq("shutdown"))
    {
        return parse_message(body)
            .map(ClientFrame::Shutdown)
            .map_err(|error| error.to_string());
    }
    Ok(ClientFrame::Forward(forward_client_body(
        editor, output, proxy, settings, body, fields,
    )))
}

fn handle_client_frame(
    session: &mut Session,
    unmanaged: bool,
    frame: ClientFrame,
) -> Result<Option<ExitCode>> {
    match frame {
        ClientFrame::Exit => Ok(Some(exit_session(session, 0)?)),
        ClientFrame::Shutdown(message) => Ok(Some(shutdown_session(session, message)?)),
        ClientFrame::Forward(Ok(())) => Ok(None),
        ClientFrame::Forward(Err(error)) => {
            crate::error!("cannot forward client message: {error}");
            if unmanaged {
                return Ok(Some(exit_session(session, 1)?));
            }
            recover(session, &error.to_string(), true)?;
            Ok(None)
        }
    }
}

fn exit_session(session: &mut Session, code: u8) -> Result<ExitCode> {
    let child = session.editor.child.take();
    cleanup_runtime(&mut session.runtime, child);
    Ok(if code == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(code)
    })
}

fn shutdown_session(session: &mut Session, message: Value) -> Result<ExitCode> {
    let bridge_id = match forward_client_request(
        &mut session.editor,
        &mut session.output,
        &mut session.proxy,
        message,
    ) {
        Ok(Some(bridge_id)) => bridge_id,
        Ok(None) => return exit_session(session, 0),
        Err(error) => {
            crate::error!("cannot forward shutdown: {error}");
            return exit_session(session, 1);
        }
    };
    loop {
        match session.editor.connection.read_frame() {
            Ok(Some(body)) => {
                let is_response = parse_message(&body)
                    .ok()
                    .and_then(|message| message.get("id").and_then(Value::as_i64))
                    == Some(bridge_id);
                forward_server_message(
                    &mut session.editor.connection.writer,
                    session.editor.lsp_port,
                    &mut session.output,
                    &mut session.proxy,
                    &body,
                    false,
                )?;
                if is_response {
                    return exit_session(session, 0);
                }
            }
            Ok(None) | Err(_) => return exit_session(session, 1),
        }
    }
}

pub(super) fn forward_initialize(
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
    if let Err(error) = send_godot(&mut editor.connection.writer, &initialize, true) {
        return Err(error.to_string());
    }
    loop {
        let frame = match editor.connection.read_frame() {
            Ok(Some(frame)) => frame,
            Ok(None) => return Err("Godot closed during initialize".to_owned()),
            Err(error) => return Err(error.to_string()),
        };
        let message = parse_message(&frame)?;
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
            if let Err(error) = send_client(output, &response) {
                return Err(error.to_string());
            }
            if !project_diagnostics && !proxy.workspace_symbols_notice_sent {
                if let Err(error) = send_info_message(
                    output,
                    "Project diagnostics are disabled; workspace symbols cover only files open in Zed.",
                ) {
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
            if send_client(output, &message).is_err() {
                return Err("cannot forward Godot request".to_owned());
            }
        } else if send_client(output, &message).is_err() {
            return Err("cannot forward Godot notification".to_owned());
        }
    }
}
