use super::*;

pub(super) fn start_recovery(session: &mut Session) -> Result<RecoveryQueue> {
    reset_for_recovery(session)?;
    fail_in_flight(session)?;
    Ok(RecoveryQueue::default())
}

fn reset_for_recovery(session: &mut Session) -> Result<()> {
    session.proxy.documents.set_open_change_events(false);
    session.proxy.symbol_cache.clear();
    session.proxy.symbol_scheduled.clear();
    session.proxy.bulk_generation += 1;
    session.proxy.project_diagnostics_started = false;
    set_recovering(&session.runtime)
}

fn fail_in_flight(session: &mut Session) -> Result<()> {
    for pending in session.proxy.pending.drain().map(|(_, pending)| pending) {
        if !pending.internal {
            send_error(
                &mut session.output,
                &pending.zed_id,
                -32803,
                "RequestFailed",
            )?;
        }
    }
    for queued in session.proxy.queued.drain(..) {
        if let Some(id) = queued.get("id") {
            send_error(&mut session.output, id, -32803, "RequestFailed")?;
        }
    }
    session.proxy.queued_bytes = 0;
    let served = session.proxy.server_requests.drain().collect::<Vec<_>>();
    session.proxy.stale_server_ids.extend(served);
    Ok(())
}

pub(super) fn recover_with_status(session: &mut Session, status: Option<ExitStatus>) -> Result<()> {
    let code = status.and_then(|status| status.code()).unwrap_or(-1);
    let count_recovery =
        session.runtime.mode != Mode::Gui || !gui_process_is_alive(&session.runtime);
    recover(session, &format!("code {code}"), count_recovery)
}

pub(super) fn recover(session: &mut Session, reason: &str, count_recovery: bool) -> Result<()> {
    if session.runtime.mode == Mode::Gui {
        session.runtime.mode = Mode::Headless;
        session
            .runtime
            .state
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .mode = Mode::Headless;
    }
    let now = Instant::now();
    if count_recovery {
        while session
            .proxy
            .recovery_times
            .front()
            .is_some_and(|time| now.duration_since(*time) > Duration::from_secs(60))
        {
            session.proxy.recovery_times.pop_front();
        }
        session.proxy.recovery_times.push_back(now);
    }
    reset_for_recovery(session)?;
    if session.proxy.recovery_times.len() >= 3 {
        let log_path = session.runtime.files.state.with_extension("godot.log");
        send_show_message(
            &mut session.output,
            &format!("Godot keeps crashing, see {}", log_path.display()),
        )?;
        let child = session.editor.child.take();
        cleanup_runtime(&mut session.runtime, child);
        crate::bail!("Godot keeps crashing");
    }
    send_show_message(
        &mut session.output,
        &format!("Godot exited ({reason}), restarting."),
    )?;
    fail_in_flight(session)?;
    session.editor.connection.close();
    if let Some(child) = session.editor.child.take() {
        terminate_editor_child(child);
    }
    let binary = resolve_godot(session.settings.godot_path.as_deref().map(Path::new))?;
    let deadline = startup_deadline(session.settings.startup_timeout_s);
    let mut candidate = None;
    let mut last_error = None;
    let mut recovery_queue = RecoveryQueue::default();
    let project = session.proxy.project.clone();
    for _ in 0..STARTUP_ATTEMPTS {
        match spawn_one(
            &binary,
            &session.settings,
            &project,
            &session.runtime,
            deadline,
        ) {
            Ok(editor) => {
                candidate = Some(editor);
                break;
            }
            Err(error) => {
                let deadline_error = matches!(error, StartupError::Deadline(_));
                last_error = Some(error);
                if deadline_error {
                    break;
                }
            }
        }
    }
    let Some(mut replacement) = candidate else {
        if let Some(error) = last_error {
            send_show_message(&mut session.output, &error.message())?;
        }
        crate::bail!("recovery failed");
    };
    if !session.watch.pending.is_empty() {
        let changes = coalesce_watcher_changes(std::mem::take(&mut session.watch.pending));
        session.watch.deadline = None;
        process_watcher_changes(
            &mut session.proxy,
            &mut session.output,
            None,
            &session.settings,
            changes,
            true,
        )?;
    }
    replay_initialize(&mut replacement, &mut session.proxy)?;
    session.editor = replacement;
    finish_recovery(session, &mut recovery_queue)?;
    Ok(())
}

pub(super) fn absorb_watcher_event(
    watch: &mut Watch,
    result: Option<std::io::Result<WatcherChange>>,
) {
    match result {
        Some(Ok(change)) => {
            watch.pending.push(change);
            if watch.pending.len() > WATCHER_PENDING_CAP {
                watch.pending = coalesce_watcher_changes(std::mem::take(&mut watch.pending));
                let rescan = watch
                    .pending
                    .iter()
                    .find(|change| change.kind == WatcherChangeKind::Rescan)
                    .cloned();
                watch
                    .pending
                    .truncate(WATCHER_PENDING_CAP - usize::from(rescan.is_some()));
                if let Some(rescan) = rescan {
                    watch.pending.push(rescan);
                }
                crate::warn!("project diagnostics watcher queue is full");
            }
            watch.deadline = Some(Instant::now() + Duration::from_millis(300));
        }
        Some(Err(error)) => crate::warn!("project diagnostics watcher error: {error}"),
        None => watch.watcher = None,
    }
}

pub(super) fn finish_recovery(session: &mut Session, queue: &mut RecoveryQueue) -> Result<()> {
    replay_open_documents(session)?;
    session.proxy.documents.set_open_change_events(true);
    start_project_diagnostics(&mut session.proxy, &session.settings);
    set_ready(&session.runtime, &session.editor)?;
    while let Some(item) = queue.items.pop_front() {
        let message = match item {
            RecoveryItem::Request(message) | RecoveryItem::Notification(message) => message,
            RecoveryItem::Response(message) => {
                if message
                    .get("id")
                    .is_some_and(|id| session.proxy.stale_server_ids.contains(&id.to_string()))
                {
                    crate::debug!("dropping response to stale Godot request");
                    continue;
                }
                message
            }
        };
        forward_client_message(
            &mut session.editor,
            &mut session.output,
            &mut session.proxy,
            &session.settings,
            message,
        )?;
    }
    flush_queued(&mut session.output, &mut session.editor, &mut session.proxy)
}

fn replay_open_documents(session: &mut Session) -> Result<()> {
    let messages = session
        .proxy
        .documents
        .open_docs
        .values_mut()
        .map(|doc| {
            doc.version = 1;
            crate::json!({
                "jsonrpc": "2.0",
                "method": "textDocument/didOpen",
                "params": {"textDocument": {"uri": (doc.uri.clone()), "languageId": "gdscript", "version": 1, "text": (doc.text.clone())}}
            })
        })
        .collect::<Vec<_>>();
    let mut deadline = Instant::now();
    for (index, message) in messages.into_iter().enumerate() {
        send_godot(&mut session.editor.connection.writer, &message, false)?;
        if (index + 1) % BULK_DOCUMENTS == 0 {
            deadline += Duration::from_millis(BULK_INTERVAL_MS);
            if let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
                thread::sleep(remaining);
            }
        }
    }
    let replayed = session
        .proxy
        .documents
        .open_docs
        .values()
        .map(|doc| DocumentEvent::Open {
            uri: doc.uri.clone(),
            generation: doc.generation,
            version: doc.version,
        })
        .collect::<Vec<_>>();
    for event in replayed {
        schedule_symbol_event(&mut session.proxy, event);
    }
    Ok(())
}

pub(super) fn queue_recovery_message(
    queue: &mut RecoveryQueue,
    output: &mut ClientWriter,
    body: &[u8],
) -> Result<()> {
    let message = parse_message(body)?;
    let size = crate::json::to_vec(&message).len();
    if message.get("method").is_some() {
        if message.get("id").is_some() {
            if queue.requests >= RECOVERY_QUEUE_CAP
                || queue.bytes.saturating_add(size) > QUEUE_BYTES_CAP
            {
                if let Some(id) = message.get("id") {
                    send_error(output, id, -32803, "RequestFailed")?;
                }
                return Ok(());
            }
            queue.requests += 1;
            queue.bytes += size;
            queue.items.push_back(RecoveryItem::Request(message));
        } else {
            if queue.notifications >= RECOVERY_QUEUE_CAP
                || queue.bytes.saturating_add(size) > QUEUE_BYTES_CAP
            {
                if let Some(index) = queue
                    .items
                    .iter()
                    .position(|item| matches!(item, RecoveryItem::Notification(_)))
                {
                    if let Some(RecoveryItem::Notification(old)) = queue.items.remove(index) {
                        queue.bytes = queue.bytes.saturating_sub(crate::json::to_vec(&old).len());
                    }
                    queue.notifications -= 1;
                    crate::warn!("dropping oldest notification from recovery queue");
                } else {
                    crate::warn!("dropping notification from full recovery queue");
                    return Ok(());
                }
            }
            queue.notifications += 1;
            queue.bytes += size;
            queue.items.push_back(RecoveryItem::Notification(message));
        }
    } else if message.get("id").is_some() {
        if queue.bytes.saturating_add(size) > QUEUE_BYTES_CAP {
            crate::warn!("dropping response from full recovery queue");
            return Ok(());
        }
        queue.bytes += size;
        queue.items.push_back(RecoveryItem::Response(message));
    } else {
        crate::warn!("dropping invalid message from recovery queue");
    }
    Ok(())
}

pub(super) fn replay_initialize(editor: &mut Editor, proxy: &mut ProxyState) -> Result<()> {
    let mut initialize = proxy.initialize.clone();
    let id = proxy.next_id;
    proxy.next_id += 1;
    if let Some(params) = initialize.get_mut("params").and_then(Value::as_object_mut) {
        params.remove("initializationOptions");
    }
    initialize["id"] = crate::json!(id);
    proxy.pending.insert(
        id,
        PendingRequest {
            zed_id: Value::Null,
            internal: true,
            symbol: None,
        },
    );
    send_godot(&mut editor.connection.writer, &initialize, true)?;
    loop {
        let body = editor
            .connection
            .read_frame()?
            .ok_or_else(|| crate::error::Error::new("Godot closed during recovery initialize"))?;
        let message = parse_message(&body)?;
        if message.get("method").and_then(Value::as_str) == Some("gdscript_client/changeWorkspace")
        {
            check_workspace(&message, &proxy.project, Some(editor.lsp_port))?;
            if message.get("id").is_some() {
                send_godot(
                    &mut editor.connection.writer,
                    &crate::json!({"jsonrpc":"2.0","id":(message["id"].clone()),"result":null}),
                    false,
                )?;
            }
            continue;
        }
        if message.get("id") == Some(&crate::json!(id)) {
            proxy.pending.remove(&id);
            if proxy.zed_initialized {
                send_godot(
                    &mut editor.connection.writer,
                    &crate::json!({"jsonrpc":"2.0","method":"initialized","params":{}}),
                    false,
                )?;
            }
            return Ok(());
        }
        if message.get("method").is_some() && message.get("id").is_some() {
            send_godot(
                &mut editor.connection.writer,
                &crate::json!({"jsonrpc":"2.0","id":(message["id"].clone()),"result":null}),
                false,
            )?;
        }
    }
}
