use super::*;

pub(super) fn connection_from_stream(stream: TcpStream) -> Connection {
    let (read, write) = stream.into_split();
    Connection {
        reader: FrameReader::new(read, GODOT_FRAME_CAP),
        writer: write,
    }
}

pub(super) fn startup_deadline(seconds: u32) -> Option<Instant> {
    (seconds != 0).then(|| Instant::now() + Duration::from_secs(u64::from(seconds)))
}

pub(super) async fn terminate_editor(mut editor: Editor) {
    if let Some(child) = editor.child.take() {
        terminate_editor_child(child).await;
    }
}

pub(super) async fn terminate_editor_child(child: GodotChild) {
    if let Err(error) = kill_group(child).await {
        crate::warn!("cannot terminate Godot process group: {error}");
    }
}

pub(super) async fn cleanup_runtime(runtime: &mut Runtime, child: Option<GodotChild>) {
    if runtime.mode == Mode::Unmanaged {
        return;
    }
    if runtime.mode == Mode::Gui {
        {
            let mut state = runtime.state.write().await;
            clear_owner_identity(&mut state);
        }
        let _ = publish(runtime).await;
        runtime.socket.take();
        if let Some(lock) = runtime.lock.take() {
            drop(lock);
        }
        return;
    }
    if let Some(child) = child {
        terminate_editor_child(child).await;
    }
    runtime.socket.take();
    cleanup_files(&runtime.files);
    if let Some(lock) = runtime.lock.take() {
        drop(lock);
    }
}

pub(super) async fn stale_cleanup(files: &ProjectFiles) {
    if let Ok(Some(state)) = read_state(&files.state) {
        if state.mode == Mode::Gui {
            let _ = std::fs::remove_file(&files.sock);
            return;
        }
        if let (Some(pid), Some(pgid), Some(ticks)) =
            (state.godot_pid, state.godot_pgid, state.godot_start_ticks)
        {
            if crate::state::pid_alive_with_ticks(pid, ticks) {
                if let Err(error) = kill_recorded(pid, pgid as i32, ticks).await {
                    crate::warn!("cannot terminate stale Godot editor: {error}");
                }
            }
        }
    }
    let _ = remove_if_stale(&files.state, &files.sock);
    cleanup_files(files);
}

pub(super) fn cleanup_files(files: &ProjectFiles) {
    let _ = std::fs::remove_file(&files.state);
    let _ = std::fs::remove_file(&files.sock);
}

pub(super) async fn update_state_spawned(
    runtime: &Runtime,
    child: &GodotChild,
    lsp_port: u16,
    dap_port: u16,
) -> Result<()> {
    {
        let mut state = runtime.state.write().await;
        state.godot_pid = Some(child.pid);
        state.godot_pgid = Some(child.pgid as u32);
        state.lsp_port = Some(lsp_port);
        state.dap_port = Some(dap_port);
        state.godot_start_ticks = Some(child.start_ticks);
    }
    publish(runtime).await
}

pub(super) async fn set_ready(runtime: &Runtime, editor: &Editor) -> Result<()> {
    {
        let mut state = runtime.state.write().await;
        state.status = Status::Ready;
        if let Some(child) = editor.child.as_ref() {
            state.godot_pid = Some(child.pid);
            state.godot_pgid = Some(child.pgid as u32);
            state.godot_start_ticks = Some(child.start_ticks);
        }
        state.lsp_port = Some(editor.lsp_port);
        state.dap_port = Some(editor.dap_port);
    }
    publish(runtime).await
}

pub(super) async fn set_recovering(runtime: &Runtime) -> Result<()> {
    runtime.state.write().await.status = Status::Recovering;
    publish(runtime).await
}

pub(super) async fn publish(runtime: &Runtime) -> Result<()> {
    let state = runtime.state.read().await.clone();
    crate::state::write_state(&runtime.files.state, &state)
        .context("cannot publish bridge state")?;
    Ok(())
}

pub(super) fn new_state(project: &Path, mode: Mode) -> State {
    let owner_pid = std::process::id();
    State {
        version: 1,
        project: project.to_string_lossy().into_owned(),
        status: Status::Starting,
        mode,
        godot_pid: None,
        godot_pgid: None,
        lsp_port: None,
        dap_port: None,
        owner_pid: Some(owner_pid),
        owner_start_ticks: start_ticks(owner_pid),
        godot_start_ticks: None,
        started_at: OffsetDateTime::now_utc()
            .format(&Rfc3339)
            .unwrap_or_default(),
        bridge_version: env!("CARGO_PKG_VERSION").to_owned(),
    }
}

pub(super) fn format_lines(prefix: &str, lines: &[String]) -> String {
    if lines.is_empty() {
        prefix.to_owned()
    } else {
        format!("{prefix}: {}", lines.join(" | "))
    }
}

pub(super) fn startup_failure_message(error: Option<&StartupError>, seconds: u32) -> String {
    match error {
        Some(StartupError::Deadline(lines)) => format!(
            "Godot did not start within {seconds}s. Last output: {}",
            lines.join(" | ")
        ),
        Some(StartupError::ChildExited(lines)) => {
            format!("Godot exited 3 times. Last output: {}", lines.join(" | "))
        }
        Some(error) => error.message(),
        None => "Godot exited 3 times".to_owned(),
    }
}

pub(super) async fn spawn_one(
    binary: &Path,
    settings: &Settings,
    project: &Path,
    runtime: &Runtime,
    deadline: Option<Instant>,
) -> std::result::Result<Editor, StartupError> {
    let lsp_port =
        pick_free_port(6005..=6999).map_err(|error| StartupError::Io(error.to_string()))?;
    let dap_port =
        pick_free_port(7005..=7999).map_err(|error| StartupError::Io(error.to_string()))?;
    let log_path = runtime.files.state.with_extension("godot.log");
    let mut child = spawn_godot(
        binary,
        &settings.extra_args,
        project,
        lsp_port,
        dap_port,
        log_path,
    )
    .map_err(|error| StartupError::Io(error.to_string()))?;
    if let Err(error) = update_state_spawned(runtime, &child, lsp_port, dap_port).await {
        terminate_editor_child(child).await;
        return Err(StartupError::Io(error.to_string()));
    }
    let stream = match wait_for_port(&mut child, lsp_port, deadline).await {
        Ok(Readiness::Ready(stream)) => stream,
        Ok(Readiness::ChildExited(_status)) => {
            return Err(StartupError::ChildExited(child.last_lines()))
        }
        Ok(Readiness::Deadline) => {
            let lines = child.last_lines();
            terminate_editor_child(child).await;
            return Err(StartupError::Deadline(lines));
        }
        Err(error) => {
            let lines = child.last_lines();
            terminate_editor_child(child).await;
            return Err(StartupError::Io(format!(
                "cannot wait for Godot LSP: {error}; {}",
                lines.join("\n")
            )));
        }
    };
    match wait_for_port(&mut child, dap_port, deadline).await {
        Ok(Readiness::Ready(_)) => Ok(Editor {
            child: Some(child),
            connection: connection_from_stream(stream),
            lsp_port,
            dap_port,
        }),
        Ok(Readiness::ChildExited(_status)) => Err(StartupError::ChildExited(child.last_lines())),
        Ok(Readiness::Deadline) => {
            let lines = child.last_lines();
            terminate_editor_child(child).await;
            Err(StartupError::Deadline(lines))
        }
        Err(error) => {
            let lines = child.last_lines();
            terminate_editor_child(child).await;
            Err(StartupError::Io(format!(
                "cannot wait for Godot DAP: {error}; {}",
                lines.join("\n")
            )))
        }
    }
}
