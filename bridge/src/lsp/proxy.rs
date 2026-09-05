use super::*;

pub(super) struct InitializeInput<'a> {
    pub(super) events: &'a Receiver<ProxyEvent>,
    pub(super) godot: &'a mut FrameState,
    pub(super) deferred: &'a mut VecDeque<ProxyEvent>,
}

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
            InitializeInput {
                events: &session.events,
                godot: &mut session.godot,
                deferred: &mut session.deferred,
            },
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
            crate::watch::watch_project_into(
                &session.proxy.project,
                session.settings.diagnose_addons,
                session.proxy.internal_sender.clone(),
                ProxyEvent::Watcher,
            )
            .context("cannot watch project")?,
        );
    }
    let gui_interval = Duration::from_millis(200);
    let mut gui_deadline = Instant::now() + gui_interval;
    loop {
        if !session.proxy.bulk_replay
            && !session.proxy.project_diagnostics_started
            && session.settings.project_diagnostics
        {
            start_project_diagnostics(&mut session.proxy, &session.settings);
        }
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
        let mut symbol_deadline = next_symbol_deadline(&session.proxy);
        if symbol_deadline.is_some_and(|deadline| deadline <= now) {
            let result = send_due_symbol_requests(&mut session.editor, &mut session.proxy);
            match result {
                Ok(next) => symbol_deadline = next,
                Err(error) => {
                    if let Some(code) = on_error(&mut session, unmanaged, &error)? {
                        return Ok(code);
                    }
                }
            }
        }
        if !session.proxy.bulk_documents.is_empty()
            && (session.proxy.bulk_documents.len() >= BULK_DOCUMENTS || session.proxy.bulk_complete)
            && bulk_can_advance(&session.proxy, now)
        {
            let result =
                pump_bulk_documents(&mut session.proxy, &mut session.editor.connection.writer);
            if let Some(code) = guard(&mut session, unmanaged, result)? {
                return Ok(code);
            }
        }
        if session
            .watch
            .deadline
            .is_some_and(|deadline| deadline <= now)
        {
            let changes = coalesce_watcher_changes(std::mem::take(&mut session.watch.pending));
            session.watch.deadline = None;
            let result = process_watcher_changes(
                &mut session.proxy,
                &mut session.output,
                Some(&mut session.editor),
                &session.settings,
                changes,
                false,
            );
            if let Some(code) = guard(&mut session, unmanaged, result)? {
                return Ok(code);
            }
        }
        let mut wait = Duration::from_secs(3600);
        let now = Instant::now();
        if let Some(deadline) = symbol_deadline {
            wait = wait.min(
                deadline
                    .checked_duration_since(now)
                    .unwrap_or(Duration::ZERO),
            );
        }
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
        if let Some(deadline) = session.proxy.bulk_deadline {
            wait = wait.min(
                deadline
                    .checked_duration_since(now)
                    .unwrap_or(Duration::ZERO),
            );
        }
        let event = session
            .deferred
            .pop_front()
            .map(Ok)
            .unwrap_or_else(|| session.events.recv_timeout(wait));
        match event {
            Ok(event) => {
                if let Some(code) = handle_event(&mut session, unmanaged, event)? {
                    return Ok(code);
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return exit_session(&mut session, 1),
        }
    }
}

fn handle_event(
    session: &mut Session,
    unmanaged: bool,
    event: ProxyEvent,
) -> Result<Option<ExitCode>> {
    match event {
        ProxyEvent::Client(event) => {
            session.client.feed(event)?;
            while let Some(body) = session.client.frames.pop_front() {
                let frame = process_client_frame(
                    &mut session.editor,
                    &mut session.output,
                    &mut session.proxy,
                    &session.settings,
                    &body,
                );
                let frame = match frame {
                    Ok(frame) => frame,
                    Err(error) => {
                        crate::error!("malformed client message: {error}");
                        return Ok(Some(exit_session(session, 1)?));
                    }
                };
                if let Some(code) = handle_client_frame(session, unmanaged, frame)? {
                    return Ok(Some(code));
                }
            }
            if session.client.eof && session.client.frames.is_empty() {
                return Ok(Some(exit_session(session, 0)?));
            }
        }
        ProxyEvent::Godot(event) => {
            let result = session.godot.feed(event);
            if let Some(code) = guard(session, unmanaged, result)? {
                return Ok(Some(code));
            }
            while let Some(body) = session.godot.frames.pop_front() {
                let result = forward_server_message(
                    &mut session.editor.connection.writer,
                    session.editor.lsp_port,
                    &mut session.output,
                    &mut session.proxy,
                    &body,
                    false,
                );
                let failed = result.is_err();
                if let Some(code) = guard(session, unmanaged, result)? {
                    return Ok(Some(code));
                }
                if failed {
                    break;
                }
            }
            if session.godot.eof && session.godot.frames.is_empty() {
                if unmanaged {
                    return Ok(Some(exit_session(session, 1)?));
                }
                let reason = session
                    .editor
                    .child
                    .as_mut()
                    .and_then(|child| child.child.try_wait().ok().flatten());
                recover_with_status(session, reason)?;
            }
        }
        ProxyEvent::Watcher(result) => {
            absorb_watcher_event(&mut session.watch, Some(result));
        }
        ProxyEvent::Internal(event) => match event {
            InternalEvent::Bulk {
                generation,
                document,
            } if generation == session.proxy.bulk_generation => {
                session.proxy.bulk_documents.push_back(document);
                if session.proxy.bulk_documents.len() >= BULK_DOCUMENTS {
                    let result = pump_bulk_documents(
                        &mut session.proxy,
                        &mut session.editor.connection.writer,
                    );
                    if let Some(code) = guard(session, unmanaged, result)? {
                        return Ok(Some(code));
                    }
                }
            }
            InternalEvent::BulkComplete { generation }
                if generation == session.proxy.bulk_generation =>
            {
                session.proxy.bulk_complete = true;
                let result =
                    pump_bulk_documents(&mut session.proxy, &mut session.editor.connection.writer);
                if let Some(code) = guard(session, unmanaged, result)? {
                    return Ok(Some(code));
                }
            }
            InternalEvent::Bulk { .. } | InternalEvent::BulkComplete { .. } => {}
        },
        ProxyEvent::Handoff(lock) => perform_handoff(session, lock)?,
    }
    Ok(None)
}

pub(super) fn receive_godot_frame(
    events: &Receiver<ProxyEvent>,
    godot: &mut FrameState,
    deferred: &mut VecDeque<ProxyEvent>,
) -> Result<Option<Vec<u8>>> {
    if let Some(body) = godot.frames.pop_front() {
        return Ok(Some(body));
    }
    if godot.eof {
        return Ok(None);
    }
    loop {
        let event = events
            .recv()
            .map_err(|_| io::Error::other("event channel is closed"))?;
        match event {
            ProxyEvent::Client(event) => deferred.push_back(ProxyEvent::Client(event)),
            ProxyEvent::Godot(event) => {
                godot.feed(event)?;
                if let Some(body) = godot.frames.pop_front() {
                    return Ok(Some(body));
                }
                if godot.eof {
                    return Ok(None);
                }
            }
            event => deferred.push_back(event),
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
            if let Some(code) = on_error(session, unmanaged, &error)? {
                return Ok(Some(code));
            }
            Ok(None)
        }
    }
}

fn on_error(
    session: &mut Session,
    unmanaged: bool,
    error: &dyn std::fmt::Display,
) -> Result<Option<ExitCode>> {
    if unmanaged {
        return Ok(Some(exit_session(session, 1)?));
    }
    recover(session, &error.to_string(), true)?;
    Ok(None)
}

fn guard<T>(session: &mut Session, unmanaged: bool, result: Result<T>) -> Result<Option<ExitCode>> {
    match result {
        Ok(_) => Ok(None),
        Err(error) => {
            crate::error!("proxy error: {error}");
            on_error(session, unmanaged, &error)
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
        match receive_godot_frame(&session.events, &mut session.godot, &mut session.deferred)? {
            Some(body) => {
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
            None => return exit_session(session, 1),
        }
    }
}

pub(super) fn forward_initialize(
    editor: &mut Editor,
    output: &mut ClientWriter,
    proxy: &mut ProxyState,
    project: &Path,
    project_diagnostics: bool,
    input: InitializeInput<'_>,
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
        let frame = receive_godot_frame(input.events, input.godot, input.deferred)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "Godot closed during initialize".to_owned())?;
        let fields = crate::json::scan_top_level(&frame).map_err(|error| error.to_string())?;
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
            if let Some(id) = fields.id {
                proxy.server_requests.insert(id.request_key());
            }
            if send_client(output, &message).is_err() {
                return Err("cannot forward Godot request".to_owned());
            }
        } else if send_client(output, &message).is_err() {
            return Err("cannot forward Godot notification".to_owned());
        }
    }
}
