use super::*;

pub(super) struct InitializeInput<'a> {
    pub(super) events: &'a Receiver<ProxyEvent>,
    pub(super) godot: &'a mut FrameState,
    pub(super) deferred: &'a mut DeferredQueue,
    pub(super) deadline: Option<Instant>,
    pub(super) deadline_seconds: u32,
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
                deadline: startup_deadline(session.settings.startup_timeout_s),
                deadline_seconds: session.settings.startup_timeout_s,
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
    }
    if session.settings.project_diagnostics {
        // A watch the platform cannot provide costs live project-wide
        // diagnostics, not the session: the initial scan and open documents
        // still work.
        match crate::watch::watch_project_into(
            &session.proxy.project,
            session.settings.diagnose_addons,
            session.proxy.internal_sender.clone(),
        ) {
            Ok(watcher) => session.watch.watcher = Some(watcher),
            Err(error) => crate::warn!(
                "cannot watch project {}: {error}; project diagnostics will not follow edits made outside the editor",
                session.proxy.project.display()
            ),
        }
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
        if let Some(code) = drain_bulk_events(&mut session, unmanaged)? {
            return Ok(code);
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
            if let Some(code) = pump_bulk(&mut session, unmanaged)? {
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
            if let Some(code) = process_watcher(&mut session, unmanaged, changes)? {
                return Ok(code);
            }
        }
        let mut wait = Duration::from_secs(3600);
        if (session.proxy.bulk_active || session.proxy.rescan_active)
            && session.proxy.bulk_documents.len() <= BULK_DOCUMENTS * 2
        {
            wait = wait.min(Duration::from_millis(10));
        }
        let now = Instant::now();
        if let Some(deadline) = symbol_deadline {
            wait = wait.min(until(now, deadline));
        }
        if session.runtime.mode == Mode::Gui {
            wait = wait.min(until(now, gui_deadline));
        }
        if let Some(deadline) = session.watch.deadline {
            wait = wait.min(until(now, deadline));
        }
        if let Some(deadline) = session.proxy.bulk_deadline {
            wait = wait.min(until(now, deadline));
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

fn until(now: Instant, deadline: Instant) -> Duration {
    deadline
        .checked_duration_since(now)
        .unwrap_or(Duration::ZERO)
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
            absorb_watcher_event(&mut session.watch, result);
        }
        ProxyEvent::Handoff(lock) => perform_handoff(session, lock)?,
    }
    Ok(None)
}

fn drain_bulk_events(session: &mut Session, unmanaged: bool) -> Result<Option<ExitCode>> {
    if bulk_documents_over_limit(session) {
        return Ok(None);
    }
    loop {
        let event = match session.bulk_events.try_recv() {
            Ok(event) => event,
            Err(mpsc::TryRecvError::Empty | mpsc::TryRecvError::Disconnected) => return Ok(None),
        };
        match event {
            InternalEvent::Bulk {
                generation,
                document,
            } if generation == session.proxy.bulk_generation => {
                session.proxy.bulk_documents.push_back(document);
                if session.proxy.bulk_documents.len() >= BULK_DOCUMENTS {
                    if let Some(code) = pump_bulk(session, unmanaged)? {
                        return Ok(Some(code));
                    }
                }
            }
            InternalEvent::BulkComplete { generation }
                if generation == session.proxy.bulk_generation =>
            {
                session.proxy.bulk_complete = true;
                if let Some(code) = pump_bulk(session, unmanaged)? {
                    return Ok(Some(code));
                }
            }
            InternalEvent::Rescan {
                generation,
                document,
            } if generation == session.proxy.rescan_generation => {
                session.proxy.rescan_keys.insert(document.key.clone());
                let change = WatcherChange {
                    kind: if session.proxy.documents.owner(&document.key).is_some() {
                        WatcherChangeKind::Modified
                    } else {
                        WatcherChangeKind::Created
                    },
                    path: document.path,
                };
                if let Some(code) = process_watcher(session, unmanaged, vec![change])? {
                    return Ok(Some(code));
                }
            }
            InternalEvent::RescanComplete { generation }
                if generation == session.proxy.rescan_generation =>
            {
                session.proxy.rescan_active = false;
                let changes = session
                    .proxy
                    .documents
                    .open_docs
                    .iter()
                    .filter(|(key, document)| {
                        document.owner == DocumentOwner::Bridge
                            && !session.proxy.rescan_keys.contains(*key)
                    })
                    .map(|(key, _)| WatcherChange {
                        kind: WatcherChangeKind::Removed,
                        path: key.clone(),
                    })
                    .collect();
                session.proxy.rescan_keys.clear();
                if let Some(code) = process_watcher(session, unmanaged, changes)? {
                    return Ok(Some(code));
                }
            }
            InternalEvent::Bulk { .. }
            | InternalEvent::BulkComplete { .. }
            | InternalEvent::Rescan { .. }
            | InternalEvent::RescanComplete { .. } => {}
        }
        if bulk_documents_over_limit(session) {
            return Ok(None);
        }
    }
}

fn bulk_documents_over_limit(session: &Session) -> bool {
    session.proxy.bulk_documents.len() > BULK_DOCUMENTS * 2
}

fn pump_bulk(session: &mut Session, unmanaged: bool) -> Result<Option<ExitCode>> {
    let result = pump_bulk_documents(&mut session.proxy, &mut session.editor.connection.writer);
    guard(session, unmanaged, result)
}

fn process_watcher(
    session: &mut Session,
    unmanaged: bool,
    changes: Vec<WatcherChange>,
) -> Result<Option<ExitCode>> {
    let result = process_watcher_changes(
        &mut session.proxy,
        &mut session.output,
        Some(&mut session.editor),
        &session.settings,
        changes,
        false,
    );
    guard(session, unmanaged, result)
}

pub(super) enum Received {
    Frame(Vec<u8>),
    Closed,
    TimedOut,
}

pub(super) fn receive_godot_frame(
    events: &Receiver<ProxyEvent>,
    godot: &mut FrameState,
    deferred: &mut DeferredQueue,
    deadline: Option<Instant>,
) -> Result<Received> {
    if let Some(body) = godot.frames.pop_front() {
        return Ok(Received::Frame(body));
    }
    if godot.eof {
        return Ok(Received::Closed);
    }
    loop {
        let event = match deadline {
            Some(deadline) => {
                match events.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                    Ok(event) => event,
                    Err(mpsc::RecvTimeoutError::Timeout) => return Ok(Received::TimedOut),
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        return Err(io::Error::other(EVENT_CHANNEL_CLOSED).into())
                    }
                }
            }
            None => events
                .recv()
                .map_err(|_| io::Error::other(EVENT_CHANNEL_CLOSED))?,
        };
        match event {
            ProxyEvent::Client(event) => deferred.push_back(ProxyEvent::Client(event))?,
            ProxyEvent::Godot(event) => {
                godot.feed(event)?;
                if let Some(body) = godot.frames.pop_front() {
                    return Ok(Received::Frame(body));
                }
                if godot.eof {
                    return Ok(Received::Closed);
                }
            }
            event => deferred.push_back(event)?,
        }
    }
}

pub(super) fn initialize_loop<T, F, G>(
    editor: &mut Editor,
    proxy: &mut ProxyState,
    project: &Path,
    input: InitializeInput<'_>,
    context: &mut T,
    on_response: F,
    mut on_other: G,
) -> Result<()>
where
    F: FnOnce(&mut Editor, &mut ProxyState, &mut T, Value, Value) -> Result<()>,
    G: FnMut(&mut Editor, &mut ProxyState, &mut T, Value) -> Result<()>,
{
    let mut initialize = proxy.initialize.clone();
    let original_id = initialize.get("id").cloned().unwrap_or(Value::Null);
    let bridge_id = proxy.next_id;
    proxy.next_id += 1;
    if let Some(params) = initialize.get_mut("params").and_then(Value::as_object_mut) {
        params.remove("initializationOptions");
        // Godot compares these against its opened project and degrades on a
        // mismatch, so point them at the resolved project, not the worktree.
        let uri = crate::root::canonical_path_to_uri(project);
        params.insert("rootUri".into(), crate::json!(uri.clone()));
        params.insert(
            "rootPath".into(),
            crate::json!(project.to_string_lossy().into_owned()),
        );
        if params.contains_key("workspaceFolders") {
            let name = project
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            params.insert(
                "workspaceFolders".into(),
                crate::json!([{"uri": uri, "name": name}]),
            );
        }
    }
    initialize["id"] = crate::json!(bridge_id);
    send_godot(&mut editor.connection.writer, &initialize, true)?;
    loop {
        let frame =
            match receive_godot_frame(input.events, input.godot, input.deferred, input.deadline)? {
                Received::Frame(frame) => frame,
                Received::Closed => crate::bail!("Godot closed during initialize"),
                Received::TimedOut => crate::bail!(
                    "Godot did not answer initialize within {}s",
                    input.deadline_seconds
                ),
            };
        let message = parse_message(&frame)?;
        if message.get("method").and_then(Value::as_str) == Some("gdscript_client/changeWorkspace")
        {
            check_workspace(&message, project, Some(editor.lsp_port))?;
            if let Some(id) = message.get("id") {
                send_godot(
                    &mut editor.connection.writer,
                    &crate::json!({"jsonrpc":"2.0","id":id,"result":null}),
                    false,
                )?;
            }
            continue;
        }
        if message.get("id") == Some(&crate::json!(bridge_id)) {
            return on_response(editor, proxy, context, message, original_id);
        }
        on_other(editor, proxy, context, message)?;
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
    let (fields, stopped) =
        crate::json::scan_top_level_until_method(body, super::client_method_intercepted)
            .map_err(|error| error.to_string())?;
    if stopped {
        let message = parse_message(body).map_err(|error| error.to_string())?;
        match message.get("method").and_then(Value::as_str) {
            Some("exit") => return Ok(ClientFrame::Exit),
            Some("shutdown") => return Ok(ClientFrame::Shutdown(message)),
            _ => {}
        }
        return Ok(ClientFrame::Forward(forward_client_body(
            editor,
            output,
            proxy,
            settings,
            body,
            fields,
            Some(message),
        )));
    }
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
        editor, output, proxy, settings, body, fields, None,
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
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match receive_godot_frame(
            &session.events,
            &mut session.godot,
            &mut session.deferred,
            Some(deadline),
        )? {
            Received::Frame(body) => {
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
            Received::Closed | Received::TimedOut => return exit_session(session, 1),
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
    initialize_loop(
        editor,
        proxy,
        project,
        input,
        output,
        |_, proxy, output, mut response, original_id| {
            response["id"] = original_id;
            patch_initialize_response(&mut response);
            send_client(output, &response)
                .map_err(|_| crate::error::Error::new("cannot forward Godot notification"))?;
            if !project_diagnostics && !proxy.workspace_symbols_notice_sent {
                send_info_message(
                    output,
                    "Project diagnostics are disabled; workspace symbols cover only files open in Zed.",
                )?;
                proxy.workspace_symbols_notice_sent = true;
            }
            proxy.initialized_forwarded = true;
            Ok(())
        },
        |_, proxy, output, message| {
            if message.get("method").is_some() && message.get("id").is_some() {
                let Some(id) = message.get("id").and_then(crate::json::value_request_key) else {
                    return Ok(());
                };
                proxy.server_requests.insert(id);
            }
            send_client(output, &message)
                .map_err(|_| crate::error::Error::new("cannot forward Godot notification"))
        },
    )
    .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn receive_godot_frame_times_out_when_godot_does_not_answer() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let godot = std::thread::spawn(move || {
            let (_stream, _) = listener.accept().unwrap();
            std::thread::sleep(Duration::from_millis(100));
        });
        let stream = TcpStream::connect(address).unwrap();
        let (sender, events) = mpsc::sync_channel(1);
        let reader =
            spawn_frame_reader("test-godot-reader", stream, sender, ProxyEvent::Godot).unwrap();
        let mut frame_state = FrameState::new(GODOT_FRAME_CAP);
        let mut deferred = DeferredQueue::default();
        let received = receive_godot_frame(
            &events,
            &mut frame_state,
            &mut deferred,
            Some(Instant::now() + Duration::from_millis(10)),
        )
        .unwrap();
        assert!(matches!(received, Received::TimedOut));
        godot.join().unwrap();
        reader.join().unwrap();
    }
}
