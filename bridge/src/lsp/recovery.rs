use super::*;

pub(super) fn reset_for_recovery(session: &mut Session) -> Result<()> {
    session.proxy.symbol_cache.clear();
    session.proxy.symbol_containers.clear();
    session.proxy.symbol_scheduled.clear();
    session.proxy.bulk_documents.clear();
    session.proxy.bulk_batch_uris.clear();
    session.proxy.bulk_complete = false;
    session.proxy.bulk_active = false;
    session.proxy.bulk_replay = false;
    session.proxy.bulk_deadline = None;
    session.proxy.rescan_generation += 1;
    session.proxy.rescan_keys.clear();
    session.proxy.rescan_active = false;
    session.godot = FrameState::new(GODOT_FRAME_CAP);
    session.proxy.bulk_generation += 1;
    session.proxy.project_diagnostics_started = false;
    set_recovering(&session.runtime)
}

pub(super) fn fail_in_flight(session: &mut Session) -> Result<()> {
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
    for (_, zed_id, _) in session.proxy.queued.drain(..) {
        send_error(&mut session.output, &zed_id, -32803, "RequestFailed")?;
    }
    session.proxy.queued_bytes = 0;
    let served = session.proxy.server_requests.drain();
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
    let project = session.proxy.project.clone();
    for _ in 0..STARTUP_ATTEMPTS {
        match spawn_one(
            &binary,
            &session.settings,
            &project,
            &session.runtime,
            &session.proxy.internal_sender,
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
    replay_initialize(
        &mut replacement,
        &mut session.proxy,
        &session.events,
        &mut session.godot,
        &mut session.deferred,
    )?;
    session.editor = replacement;
    finish_recovery(session, &mut RecoveryQueue::default())?;
    Ok(())
}

pub(super) fn absorb_watcher_event(watch: &mut Watch, result: std::io::Result<WatcherChange>) {
    match result {
        Ok(change) => {
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
        Err(error) => crate::warn!("project diagnostics watcher error: {error}"),
    }
}

pub(super) fn finish_recovery(session: &mut Session, queue: &mut RecoveryQueue) -> Result<()> {
    replay_open_documents(session)?;
    if !session.proxy.bulk_replay {
        start_project_diagnostics(&mut session.proxy, &session.settings);
    }
    set_ready(&session.runtime, &session.editor)?;
    while let Some(item) = queue.items.pop_front() {
        let message = match item {
            RecoveryItem::Request(message, size)
            | RecoveryItem::Notification(message, size)
            | RecoveryItem::Response(message, size) => {
                queue.bytes = queue.bytes.saturating_sub(size);
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
    flush_queued(
        &mut session.output,
        &mut session.editor.connection.writer,
        &mut session.proxy,
    )
}

fn replay_open_documents(session: &mut Session) -> Result<()> {
    session.proxy.bulk_generation += 1;
    session.proxy.bulk_documents.clear();
    session.proxy.bulk_batch_uris.clear();
    session.proxy.bulk_complete = true;
    session.proxy.bulk_active = true;
    session.proxy.bulk_replay = true;
    session.proxy.bulk_deadline = None;
    for (key, doc) in &mut session.proxy.documents.open_docs {
        doc.version = 1;
        session
            .proxy
            .bulk_documents
            .push_back(docs_state::ScannedDocument {
                path: key.clone(),
                key: key.clone(),
                text: None,
            });
    }
    let replayed = session
        .proxy
        .documents
        .open_docs
        .values()
        .map(|doc| (doc.uri.clone(), doc.generation, doc.version))
        .collect::<Vec<_>>();
    for (uri, generation, version) in replayed {
        schedule_symbols(&mut session.proxy, &uri, generation, version);
    }
    pump_bulk_documents(&mut session.proxy, &mut session.editor.connection.writer)
}

pub(super) fn queue_recovery_message(
    queue: &mut RecoveryQueue,
    output: &mut ClientWriter,
    body: &[u8],
) -> Result<()> {
    let message = parse_message(body)?;
    let size = body.len();
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
            queue.items.push_back(RecoveryItem::Request(message, size));
        } else {
            while queue.notifications >= RECOVERY_QUEUE_CAP
                || queue.bytes.saturating_add(size) > QUEUE_BYTES_CAP
            {
                if let Some(index) = queue
                    .items
                    .iter()
                    .position(|item| matches!(item, RecoveryItem::Notification(_, _)))
                {
                    if let Some(RecoveryItem::Notification(_, old_size)) = queue.items.remove(index)
                    {
                        queue.bytes = queue.bytes.saturating_sub(old_size);
                    }
                    queue.notifications -= 1;
                    crate::warn!("dropping oldest notification from recovery queue");
                } else {
                    crate::warn!("dropping notification from full recovery queue");
                    return Ok(());
                }
            }
            if queue.bytes.saturating_add(size) > QUEUE_BYTES_CAP {
                crate::warn!("dropping notification from full recovery queue");
                return Ok(());
            }
            queue.notifications += 1;
            queue.bytes += size;
            queue
                .items
                .push_back(RecoveryItem::Notification(message, size));
        }
    } else if message.get("id").is_some() {
        if queue.bytes.saturating_add(size) > QUEUE_BYTES_CAP {
            crate::warn!("dropping response from full recovery queue");
            return Ok(());
        }
        queue.bytes += size;
        queue.items.push_back(RecoveryItem::Response(message, size));
    } else {
        crate::warn!("dropping invalid message from recovery queue");
    }
    Ok(())
}

pub(super) fn replay_initialize(
    editor: &mut Editor,
    proxy: &mut ProxyState,
    events: &Receiver<ProxyEvent>,
    godot: &mut FrameState,
    deferred: &mut DeferredQueue,
) -> Result<()> {
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
        let body = match receive_godot_frame(events, godot, deferred)? {
            Some(body) => body,
            None => {
                if let Some(child) = editor.child.as_mut() {
                    crate::debug!(
                        "recovery Godot status: {:?}; output: {:?}",
                        child.child.try_wait(),
                        child.last_lines()
                    );
                }
                return Err(crate::error::Error::new(
                    "Godot closed during recovery initialize",
                ));
            }
        };
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
