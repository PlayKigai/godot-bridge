use crate::error::{Error, Result};
use crate::json::Value;
use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use crate::godot_bin::{check_version, resolve_godot};
use crate::process::{kill_recorded, pick_free_port, port_listener_belongs_to_process, spawn_gui};
use crate::root::{cwd_root, find_project_dir};
use crate::settings_file::{load_zed_settings, Settings};
use crate::state::{
    detached_gui_state, gui_process_alive, matches_project, read_state, socket_request, try_lock,
    write_state, Mode, ProjectFiles, State, Status,
};

const SOCKET_TIMEOUT: Duration = Duration::from_secs(5);
const POLL_INTERVAL: Duration = Duration::from_millis(200);

enum PortReadiness {
    Ready,
    Dead,
    Deadline,
}

pub fn run(file: &Path) -> Result<ExitCode> {
    let root = cwd_root()?;
    let settings = load_zed_settings(&root)?;
    let project = find_project_dir(
        &root,
        Some(file),
        settings.project_dir.as_deref().map(Path::new),
    )?;
    let files = ProjectFiles::new(&project)?;

    if let Some(response) = try_handoff_with_timeout(&files, &project, SOCKET_TIMEOUT) {
        return print_response(response);
    }

    let lock = match try_lock(&files.lock)? {
        Some(lock) => lock,
        None => return retry_handoff(&files, &project),
    };
    let result = launch_or_reuse(&files, &project, &settings);
    drop(lock);
    result
}

fn retry_handoff(files: &ProjectFiles, project: &Path) -> Result<ExitCode> {
    let deadline = Instant::now() + SOCKET_TIMEOUT;
    loop {
        if let Some(response) = try_handoff_with_timeout(files, project, Duration::from_millis(250))
        {
            return print_response(response);
        }
        if Instant::now() >= deadline {
            crate::bail!("An owner exists but does not answer");
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

fn try_handoff_with_timeout(
    files: &ProjectFiles,
    project: &Path,
    timeout: Duration,
) -> Option<Value> {
    socket_request(
        &files.sock,
        &crate::json!({"cmd": "handoff", "project": (project.to_string_lossy().into_owned())}),
        timeout,
    )
    .ok()
}

fn print_response(response: Value) -> Result<ExitCode> {
    println!("{response}");
    if response
        .get("accepted")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        Ok(ExitCode::SUCCESS)
    } else {
        Ok(ExitCode::from(1))
    }
}

fn launch_or_reuse(files: &ProjectFiles, project: &Path, settings: &Settings) -> Result<ExitCode> {
    let existing = read_state(&files.state)?;
    if let Some(mut state) = existing {
        if !matches_project(&state, project) {
            crate::bail!("project mismatch");
        }
        if state.mode == Mode::Gui && gui_process_alive(&state) {
            let (pid, ticks, lsp_port, dap_port) = recorded_gui(&state)?;
            match wait_for_ports(pid, ticks, lsp_port, dap_port, settings.startup_timeout_s) {
                PortReadiness::Ready => {
                    state.status = Status::Ready;
                    state.owner_pid = None;
                    state.owner_start_ticks = None;
                    write_state(&files.state, &state)?;
                    return print_state(state);
                }
                PortReadiness::Dead => {}
                PortReadiness::Deadline => {
                    state.owner_pid = None;
                    state.owner_start_ticks = None;
                    write_state(&files.state, &state)?;
                    crate::bail!("GUI editor {pid} is not answering on its ports");
                }
            }
        }
        remove_files(files);
    }

    let binary = resolve_godot(settings.godot_path.as_deref().map(Path::new))?;
    check_version(&binary)?;
    let lsp_port = pick_free_port(6005..=6999)?;
    let dap_port = pick_free_port(7005..=7999)?;
    let (pid, pgid, ticks) = spawn_gui(
        &binary,
        &settings.extra_args,
        project,
        lsp_port,
        dap_port,
        files.state.with_extension("gui.log"),
    )?;
    let mut state = detached_gui_state(project, Status::Starting);
    state.godot_pid = Some(pid);
    state.godot_pgid = Some(pgid as u32);
    state.godot_start_ticks = Some(ticks);
    state.lsp_port = Some(lsp_port);
    state.dap_port = Some(dap_port);
    write_state(&files.state, &state)?;

    match wait_for_ports(pid, ticks, lsp_port, dap_port, settings.startup_timeout_s) {
        PortReadiness::Ready => {
            state.status = Status::Ready;
            write_state(&files.state, &state)?;
            print_state(state)
        }
        PortReadiness::Dead | PortReadiness::Deadline => {
            let _ = kill_recorded(pid, pgid, ticks);
            let tail = log_tail(&files.state.with_extension("gui.log"));
            remove_files(files);
            let message = match state.status {
                Status::Starting => "GUI editor did not start",
                Status::Ready | Status::Recovering => "GUI editor exited",
            };
            crate::bail!("{message}: {tail}")
        }
    }
}

fn recorded_gui(state: &State) -> Result<(u32, u64, u16, u16)> {
    let pid = state
        .godot_pid
        .ok_or_else(|| Error::new("detached GUI state has no process id"))?;
    let ticks = state
        .godot_start_ticks
        .ok_or_else(|| Error::new("detached GUI state has no process identity"))?;
    let lsp_port = state
        .lsp_port
        .ok_or_else(|| Error::new("detached GUI state has no LSP port"))?;
    let dap_port = state
        .dap_port
        .ok_or_else(|| Error::new("detached GUI state has no DAP port"))?;
    Ok((pid, ticks, lsp_port, dap_port))
}

fn wait_for_ports(
    pid: u32,
    ticks: u64,
    lsp_port: u16,
    dap_port: u16,
    timeout_seconds: u32,
) -> PortReadiness {
    let deadline = (timeout_seconds != 0)
        .then(|| Instant::now() + Duration::from_secs(u64::from(timeout_seconds)));
    loop {
        if !crate::state::pid_alive_with_ticks(pid, ticks) {
            return PortReadiness::Dead;
        }
        let lsp = SocketAddr::from(([127, 0, 0, 1], lsp_port));
        let dap = SocketAddr::from(([127, 0, 0, 1], dap_port));
        if TcpStream::connect_timeout(&lsp, POLL_INTERVAL).is_ok()
            && TcpStream::connect_timeout(&dap, POLL_INTERVAL).is_ok()
            && matches!(port_listener_belongs_to_process(pid, lsp_port), Ok(true))
            && matches!(port_listener_belongs_to_process(pid, dap_port), Ok(true))
        {
            return PortReadiness::Ready;
        }
        if deadline.is_some_and(|limit| Instant::now() >= limit) {
            return PortReadiness::Deadline;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

fn print_state(state: State) -> Result<ExitCode> {
    println!("{}", state.to_value());
    Ok(ExitCode::SUCCESS)
}

fn remove_files(files: &ProjectFiles) {
    let _ = std::fs::remove_file(&files.state);
    let _ = std::fs::remove_file(&files.sock);
}

fn log_tail(path: &Path) -> String {
    let Ok(file) = std::fs::File::open(path) else {
        return String::new();
    };
    let Ok(length) = file.metadata().map(|metadata| metadata.len()) else {
        return String::new();
    };
    let mut reader = std::io::BufReader::new(file);
    let offset = length.saturating_sub(64 * 1024);
    if std::io::Seek::seek(&mut reader, std::io::SeekFrom::Start(offset)).is_err() {
        return String::new();
    }
    let mut contents = String::new();
    if std::io::Read::read_to_string(&mut reader, &mut contents).is_err() {
        return String::new();
    }
    contents
        .lines()
        .rev()
        .take(20)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join(" | ")
}
