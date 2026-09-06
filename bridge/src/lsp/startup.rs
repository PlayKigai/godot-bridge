use super::*;
use crate::framing::connect_with_reader;

pub(super) fn connection_from_stream(
    stream: TcpStream,
    sender: mpsc::SyncSender<ProxyEvent>,
) -> io::Result<Connection> {
    connect_with_reader(stream, "godot-bridge-lsp-reader", sender, ProxyEvent::Godot)
}

pub(super) fn startup_deadline(seconds: u32) -> Option<Instant> {
    (seconds != 0).then(|| Instant::now() + Duration::from_secs(u64::from(seconds)))
}

pub(super) fn terminate_editor(mut editor: Editor) {
    if let Some(child) = editor.child.take() {
        terminate_editor_child(child);
    }
}

pub(super) fn terminate_editor_child(child: GodotChild) {
    if let Err(error) = kill_group(child) {
        crate::warn!("cannot terminate Godot process group: {error}");
    }
}

pub(super) fn cleanup_runtime(runtime: &mut Runtime, child: Option<GodotChild>) {
    if runtime.mode == Mode::Unmanaged {
        return;
    }
    if runtime.mode == Mode::Gui {
        {
            let mut state = runtime
                .state
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            clear_owner_identity(&mut state);
        }
        let _ = publish(runtime);
        runtime.socket.take();
        if let Some(lock) = runtime.lock.take() {
            drop(lock);
        }
        return;
    }
    if let Some(child) = child {
        terminate_editor_child(child);
    }
    runtime.socket.take();
    cleanup_files(&runtime.files);
    if let Some(lock) = runtime.lock.take() {
        drop(lock);
    }
}

pub(super) fn stale_cleanup(files: &ProjectFiles, project: &Path) {
    if let Ok(Some(state)) = read_state(&files.state) {
        if state.mode == Mode::Gui {
            let _ = crate::sys::remove_socket(&files.sock);
            return;
        }
        if !matches_project(&state, project) {
            return;
        }
        if let (Some(pid), Some(pgid), Some(ticks)) =
            (state.godot_pid, state.godot_pgid, state.godot_start_ticks)
        {
            if crate::state::pid_alive_with_ticks(pid, ticks) {
                if let Err(error) = kill_recorded(pid, pgid, ticks) {
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
    let _ = crate::sys::remove_socket(&files.sock);
}

pub(super) fn update_state_spawned(
    runtime: &Runtime,
    child: &GodotChild,
    lsp_port: u16,
    dap_port: u16,
) -> Result<()> {
    {
        let mut state = runtime
            .state
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.godot_pid = Some(child.pid);
        state.godot_pgid = Some(child.pgid);
        state.lsp_port = Some(lsp_port);
        state.dap_port = Some(dap_port);
        state.godot_start_ticks = Some(child.start_ticks);
    }
    publish(runtime)
}

pub(super) fn set_ready(runtime: &Runtime, editor: &Editor) -> Result<()> {
    {
        let mut state = runtime
            .state
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.status = Status::Ready;
        if let Some(child) = editor.child.as_ref() {
            state.godot_pid = Some(child.pid);
            state.godot_pgid = Some(child.pgid);
            state.godot_start_ticks = Some(child.start_ticks);
        }
        state.lsp_port = Some(editor.lsp_port);
        state.dap_port = Some(editor.dap_port);
    }
    publish(runtime)
}

pub(super) fn set_recovering(runtime: &Runtime) -> Result<()> {
    runtime
        .state
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .status = Status::Recovering;
    publish(runtime)
}

pub(super) fn publish(runtime: &Runtime) -> Result<()> {
    let state = runtime
        .state
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
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
        started_at: crate::clock::now_rfc3339(),
        bridge_version: env!("CARGO_PKG_VERSION").to_owned(),
    }
}

pub(super) fn startup_failure_message(error: Option<&StartupError>, seconds: u32) -> String {
    match error {
        Some(StartupError::Deadline(lines)) => format!(
            "Godot did not start within {seconds}s. Last output: {}",
            lines.join(" | ")
        ),
        Some(StartupError::ChildExited(lines)) => {
            format!(
                "Godot exited {STARTUP_ATTEMPTS} times. Last output: {}",
                lines.join(" | ")
            )
        }
        Some(StartupError::PortMismatch) => "Godot port belongs to another process".to_owned(),
        Some(StartupError::Io(error)) => error.clone(),
        None => format!("Godot exited {STARTUP_ATTEMPTS} times"),
    }
}

pub(super) fn spawn_one(
    binary: &Path,
    settings: &Settings,
    project: &Path,
    runtime: &Runtime,
    event_sender: &mpsc::SyncSender<ProxyEvent>,
    deadline: Option<Instant>,
) -> std::result::Result<Editor, StartupError> {
    let mut port_mismatches = 0;
    loop {
        let lsp_port =
            pick_free_port(6005..=6999).map_err(|error| StartupError::Io(error.to_string()))?;
        let dap_port =
            pick_free_port(7005..=7999).map_err(|error| StartupError::Io(error.to_string()))?;
        let log_path = runtime.files.state.with_extension("godot.log");
        let child = spawn_godot(
            binary,
            &settings.extra_args,
            project,
            lsp_port,
            dap_port,
            log_path,
        )
        .map_err(|error| StartupError::Io(error.to_string()))?;
        if let Err(error) = update_state_spawned(runtime, &child, lsp_port, dap_port) {
            terminate_editor_child(child);
            return Err(StartupError::Io(error.to_string()));
        }
        let (child, stream) = match await_port(child, lsp_port, deadline, "LSP") {
            Ok(value) => value,
            Err(StartupError::PortMismatch) => {
                port_mismatches += 1;
                if port_mismatches >= STARTUP_ATTEMPTS {
                    return Err(StartupError::PortMismatch);
                }
                continue;
            }
            Err(error) => return Err(error),
        };
        let (child, _) = match await_port(child, dap_port, deadline, "DAP") {
            Ok(value) => value,
            Err(StartupError::PortMismatch) => {
                port_mismatches += 1;
                if port_mismatches >= STARTUP_ATTEMPTS {
                    return Err(StartupError::PortMismatch);
                }
                continue;
            }
            Err(error) => return Err(error),
        };
        return Ok(Editor {
            child: Some(child),
            connection: connection_from_stream(stream, event_sender.clone())
                .map_err(|error| StartupError::Io(error.to_string()))?,
            lsp_port,
            dap_port,
        });
    }
}

fn await_port(
    mut child: GodotChild,
    port: u16,
    deadline: Option<Instant>,
    label: &str,
) -> std::result::Result<(GodotChild, TcpStream), StartupError> {
    match wait_for_port(&mut child, port, deadline) {
        Ok(Readiness::Ready(stream)) => match port_listener_belongs_to_process(child.pid, port) {
            Ok(true) => Ok((child, stream)),
            Ok(false) => {
                crate::warn!("Godot {label} port {port} belongs to another process");
                terminate_editor_child(child);
                Err(StartupError::PortMismatch)
            }
            Err(error) => {
                let lines = child.last_lines();
                terminate_editor_child(child);
                Err(StartupError::Io(format!(
                    "cannot verify Godot {label} port: {error}; {}",
                    lines.join("\n")
                )))
            }
        },
        Ok(Readiness::ChildExited(_)) => Err(StartupError::ChildExited(child.last_lines())),
        Ok(Readiness::Deadline) => {
            let lines = child.last_lines();
            terminate_editor_child(child);
            Err(StartupError::Deadline(lines))
        }
        Err(error) => {
            let lines = child.last_lines();
            terminate_editor_child(child);
            Err(StartupError::Io(format!(
                "cannot wait for Godot {label}: {error}; {}",
                lines.join("\n")
            )))
        }
    }
}
