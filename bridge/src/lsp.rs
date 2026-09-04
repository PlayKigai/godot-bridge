use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::process::{ExitCode, ExitStatus};
use std::sync::Arc;
use std::time::{Duration, Instant};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use tokio::io::{AsyncWrite, AsyncWriteExt, BufWriter};
use tokio::net::{tcp::OwnedWriteHalf, TcpStream};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::sync::RwLock;

use crate::docs_state::{
    self, DocumentAction, DocumentEvent, DocumentOwner, DocumentState, ProjectWatcher,
    WatcherChange, WatcherChangeKind, BULK_DOCUMENTS, BULK_INTERVAL_MS,
};
use crate::framing::{write_frame, FrameReader};
use crate::godot_bin::{check_version, resolve_godot};
use crate::process::{
    kill_group, kill_recorded, pick_free_port, spawn_godot, spawn_gui, wait_for_port, GodotChild,
    Readiness,
};
use crate::root::{find_project_dir, worktree_root_from_initialize};
use crate::settings_file::{parse_settings, Settings};
use crate::state::{
    clear_owner_identity, gui_process_alive, handoff_decision, read_state, remove_if_stale,
    serve_socket, set_owner_identity, start_ticks, try_lock, write_state, HandoffDecision,
    LockGuard, Mode, ProjectFiles, State, Status,
};
use crate::symbols::{self, Symbol};

const CLIENT_FRAME_CAP: usize = 64 * 1024 * 1024;
const GODOT_FRAME_CAP: usize = 64 * 1024 * 1024;
const GODOT_WRITE_CAP: usize = 4 * 1024 * 1024;
const IN_FLIGHT_CAP: usize = 32;
const RECOVERY_QUEUE_CAP: usize = 1000;
const STARTUP_ATTEMPTS: usize = 3;

type ClientWriter = BufWriter<tokio::io::Stdout>;

struct PendingRequest {
    zed_id: Value,
    internal: bool,
    symbol: Option<(String, i64)>,
}

struct ProxyState {
    documents: DocumentState,
    pending: HashMap<i64, PendingRequest>,
    queued: VecDeque<Value>,
    server_requests: HashSet<String>,
    stale_server_ids: HashSet<String>,
    next_id: i64,
    initialized_forwarded: bool,
    zed_initialized: bool,
    initialize: Value,
    project: PathBuf,
    recovery_times: VecDeque<Instant>,
    project_diagnostics_started: bool,
    workspace_symbols_notice_sent: bool,
    symbol_events: UnboundedReceiver<DocumentEvent>,
    symbol_cache: HashMap<String, Vec<Symbol>>,
    symbol_scheduled: HashMap<String, (i64, Instant)>,
}

struct Connection {
    reader: FrameReader<tokio::net::tcp::OwnedReadHalf>,
    writer: OwnedWriteHalf,
}

struct Editor {
    child: Option<GodotChild>,
    connection: Connection,
    lsp_port: u16,
    dap_port: u16,
}

struct Runtime {
    files: ProjectFiles,
    state: Arc<RwLock<State>>,
    socket: Option<crate::state::SocketHandle>,
    lock: Option<crate::state::LockGuard>,
    handoff_receiver: Option<UnboundedReceiver<HandoffRequest>>,
    mode: Mode,
}

struct HandoffRequest {
    dap_lock: HandoffLock,
}

struct HandoffLock {
    guard: Option<LockGuard>,
    path: PathBuf,
}

impl Drop for HandoffLock {
    fn drop(&mut self) {
        self.guard.take();
        crate::state::remove_lock_file(&self.path);
    }
}

struct InitFailure {
    editor: Box<Editor>,
    code: i64,
    message: String,
}

enum RecoveryItem {
    Request(Value),
    Notification(Value),
    Response(Value),
}

#[derive(Default)]
struct RecoveryQueue {
    items: VecDeque<RecoveryItem>,
    requests: usize,
    notifications: usize,
}

enum StartupError {
    ChildExited(Option<i32>, Vec<String>),
    Deadline(Vec<String>),
    Io(String),
}

impl StartupError {
    fn message(&self) -> String {
        match self {
            Self::ChildExited(_, lines) => format_lines("Godot exited", lines),
            Self::Deadline(lines) => format_lines("Godot did not start before the deadline", lines),
            Self::Io(error) => error.clone(),
        }
    }
}

enum GuiReconnect {
    Ready {
        state: State,
        connection: Connection,
    },
    Dead,
    Deadline {
        pid: u32,
        state: State,
    },
}

async fn serve_owner_socket(
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
                                        path: dap_path,
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

async fn reconnect_gui(
    files: &ProjectFiles,
    mut state: State,
    timeout_seconds: u32,
) -> Result<GuiReconnect> {
    let Some(pid) = state.godot_pid else {
        cleanup_files(files);
        return Ok(GuiReconnect::Dead);
    };
    let Some(ticks) = state.godot_start_ticks else {
        cleanup_files(files);
        return Ok(GuiReconnect::Dead);
    };
    let Some(lsp_port) = state.lsp_port else {
        cleanup_files(files);
        return Ok(GuiReconnect::Dead);
    };
    let Some(dap_port) = state.dap_port else {
        cleanup_files(files);
        return Ok(GuiReconnect::Dead);
    };
    match wait_for_detached_ports(
        pid,
        ticks,
        lsp_port,
        dap_port,
        startup_deadline(timeout_seconds),
    )
    .await
    {
        DetachedPorts::Ready(stream) => {
            state.status = Status::Ready;
            state.mode = Mode::Gui;
            set_owner_identity(&mut state);
            Ok(GuiReconnect::Ready {
                state,
                connection: connection_from_stream(stream),
            })
        }
        DetachedPorts::Dead => {
            cleanup_files(files);
            Ok(GuiReconnect::Dead)
        }
        DetachedPorts::Deadline => {
            clear_owner_identity(&mut state);
            let _ = std::fs::remove_file(&files.sock);
            Ok(GuiReconnect::Deadline { pid, state })
        }
    }
}

enum DetachedPorts {
    Ready(TcpStream),
    Dead,
    Deadline,
}

async fn wait_for_detached_ports(
    pid: u32,
    ticks: u64,
    lsp_port: u16,
    dap_port: u16,
    deadline: Option<Instant>,
) -> DetachedPorts {
    loop {
        if !crate::state::pid_alive_with_ticks(pid, ticks) {
            return DetachedPorts::Dead;
        }
        if let Ok(lsp) = TcpStream::connect(("127.0.0.1", lsp_port)).await {
            if TcpStream::connect(("127.0.0.1", dap_port)).await.is_ok() {
                return DetachedPorts::Ready(lsp);
            }
        }
        if deadline.is_some_and(|limit| Instant::now() >= limit) {
            return DetachedPorts::Deadline;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

async fn wait_for_ports_closed(lsp_port: u16, dap_port: u16) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let lsp_closed = TcpStream::connect(("127.0.0.1", lsp_port)).await.is_err();
        let dap_closed = TcpStream::connect(("127.0.0.1", dap_port)).await.is_err();
        if lsp_closed && dap_closed || Instant::now() >= deadline {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[allow(clippy::too_many_arguments)]
async fn wait_for_detached_ports_during_handoff(
    pid: u32,
    ticks: u64,
    lsp_port: u16,
    dap_port: u16,
    deadline: Option<Instant>,
    input: &mut FrameReader<tokio::io::Stdin>,
    output: &mut ClientWriter,
    queue: &mut RecoveryQueue,
) -> Result<DetachedPorts> {
    loop {
        if !crate::state::pid_alive_with_ticks(pid, ticks) {
            return Ok(DetachedPorts::Dead);
        }
        if let Ok(lsp) = TcpStream::connect(("127.0.0.1", lsp_port)).await {
            if TcpStream::connect(("127.0.0.1", dap_port)).await.is_ok() {
                return Ok(DetachedPorts::Ready(lsp));
            }
        }
        if deadline.is_some_and(|limit| Instant::now() >= limit) {
            return Ok(DetachedPorts::Deadline);
        }
        let sleep_for = deadline.map_or(Duration::from_millis(200), |limit| {
            limit
                .checked_duration_since(Instant::now())
                .unwrap_or(Duration::ZERO)
                .min(Duration::from_millis(200))
        });
        tokio::select! {
            client = input.read_frame() => {
                match client {
                    Ok(Some(body)) => queue_recovery_message(queue, output, &body).await?,
                    Ok(None) => return Err(anyhow!("Zed closed during GUI handoff")),
                    Err(error) => return Err(anyhow!(error.to_string())),
                }
            }
            _ = tokio::time::sleep(sleep_for) => {}
        }
    }
}

async fn next_handoff(
    receiver: &mut Option<UnboundedReceiver<HandoffRequest>>,
) -> Option<HandoffRequest> {
    match receiver.as_mut() {
        Some(receiver) => receiver.recv().await,
        None => std::future::pending().await,
    }
}

async fn gui_process_is_alive(runtime: &Runtime) -> bool {
    let state = runtime.state.read().await;
    gui_process_alive(&state)
}

#[allow(clippy::too_many_arguments)]
async fn perform_handoff(
    input: &mut FrameReader<tokio::io::Stdin>,
    output: &mut ClientWriter,
    proxy: &mut ProxyState,
    runtime: &mut Runtime,
    settings: &Settings,
    editor: &mut Editor,
    watcher: &mut Option<ProjectWatcher>,
    watcher_pending: &mut Vec<WatcherChange>,
    watcher_deadline: &mut Option<Instant>,
    handoff: HandoffRequest,
) -> Result<()> {
    let _dap_lock = handoff.dap_lock;
    let mut recovery_queue = start_recovery(output, proxy, runtime).await?;
    let old_lsp_port = editor.lsp_port;
    let old_dap_port = editor.dap_port;
    let mut gui_identity = None;
    let swap = async {
        if let Some(child) = editor.child.take() {
            kill_group(child).await?;
        }
        wait_for_ports_closed(old_lsp_port, old_dap_port).await;
        let binary = resolve_godot(settings.godot_path.as_deref().map(Path::new))
            .map_err(|error| anyhow!(error))?;
        check_version(&binary).map_err(|error| anyhow!(error))?;
        let (pid, pgid, ticks) = spawn_gui(
            &binary,
            &settings.extra_args,
            &proxy.project,
            old_lsp_port,
            old_dap_port,
            runtime.files.state.with_extension("gui.log"),
        )?;
        gui_identity = Some((pid, pgid, ticks));
        runtime.mode = Mode::Gui;
        {
            let mut state = runtime.state.write().await;
            state.status = Status::Starting;
            state.mode = Mode::Gui;
            state.godot_pid = Some(pid);
            state.godot_pgid = Some(pgid as u32);
            state.godot_start_ticks = Some(ticks);
            state.lsp_port = Some(old_lsp_port);
            state.dap_port = Some(old_dap_port);
        }
        publish(runtime).await?;
        let connection = match wait_for_detached_ports_during_handoff(
            pid,
            ticks,
            old_lsp_port,
            old_dap_port,
            startup_deadline(settings.startup_timeout_s),
            input,
            output,
            &mut recovery_queue,
        )
        .await?
        {
            DetachedPorts::Ready(stream) => connection_from_stream(stream),
            DetachedPorts::Dead => return Err(anyhow!("GUI editor exited during handoff")),
            DetachedPorts::Deadline => {
                return Err(anyhow!(
                    "GUI editor {pid} is not answering on its ports"
                ))
            }
        };
        let mut replacement = Editor {
            child: None,
            connection,
            lsp_port: old_lsp_port,
            dap_port: old_dap_port,
        };
        replay_initialize(&mut replacement, proxy).await?;
        for doc in proxy.documents.open_docs.values_mut() {
            doc.version = 1;
            let message = json!({
                "jsonrpc": "2.0",
                "method": "textDocument/didOpen",
                "params": {"textDocument": {"uri": doc.uri, "languageId": "gdscript", "version": 1, "text": doc.text}}
            });
            send_godot(&mut replacement.connection.writer, &message, false).await?;
        }
        let replayed = proxy
            .documents
            .open_docs
            .values()
            .map(|doc| DocumentEvent::Open {
                uri: doc.uri.clone(),
                version: doc.version,
            })
            .collect::<Vec<_>>();
        for event in replayed {
            schedule_symbol_event(proxy, event);
        }
        proxy.documents.set_open_change_events(true);
        start_project_diagnostics(&mut replacement, proxy, settings).await?;
        *editor = replacement;
        set_ready(runtime, editor).await?;
        while let Some(item) = recovery_queue.items.pop_front() {
            match item {
                RecoveryItem::Request(message) | RecoveryItem::Notification(message) => {
                    forward_client_message(editor, output, proxy, settings, message).await?;
                }
                RecoveryItem::Response(message) => {
                    let stale = message
                        .get("id")
                        .is_some_and(|id| proxy.stale_server_ids.contains(&id.to_string()));
                    if !stale {
                        forward_client_message(editor, output, proxy, settings, message).await?;
                    }
                }
            }
        }
        flush_queued(output, editor, proxy).await
    }
    .await;

    match swap {
        Ok(()) => Ok(()),
        Err(error) => {
            send_show_message(output, &error.to_string()).await?;
            if let Some((pid, pgid, ticks)) = gui_identity {
                let _ = kill_recorded(pid, pgid, ticks).await;
            }
            fail_recovery_queue(output, &mut recovery_queue).await?;
            runtime.mode = Mode::Headless;
            runtime.state.write().await.mode = Mode::Headless;
            recover(
                input,
                output,
                proxy,
                runtime,
                settings,
                editor,
                watcher,
                watcher_pending,
                watcher_deadline,
                "GUI handoff failed",
                true,
            )
            .await
        }
    }
}

async fn fail_recovery_queue(output: &mut ClientWriter, queue: &mut RecoveryQueue) -> Result<()> {
    while let Some(item) = queue.items.pop_front() {
        if let RecoveryItem::Request(message) = item {
            if let Some(id) = message.get("id") {
                send_error(output, id, -32803, "RequestFailed").await?;
            }
        }
    }
    Ok(())
}

async fn start_recovery(
    output: &mut ClientWriter,
    proxy: &mut ProxyState,
    runtime: &mut Runtime,
) -> Result<RecoveryQueue> {
    proxy.documents.set_open_change_events(false);
    proxy.symbol_cache.clear();
    set_recovering(runtime).await?;
    proxy.project_diagnostics_started = false;
    for pending in proxy.pending.drain().map(|(_, pending)| pending) {
        if !pending.internal {
            send_error(output, &pending.zed_id, -32803, "RequestFailed").await?;
        }
    }
    for queued in proxy.queued.drain(..) {
        if let Some(id) = queued.get("id") {
            send_error(output, id, -32803, "RequestFailed").await?;
        }
    }
    proxy.stale_server_ids.extend(proxy.server_requests.drain());
    Ok(RecoveryQueue::default())
}

pub async fn run(trailing: Vec<String>) -> Result<ExitCode> {
    let _ = trailing;
    let mut input = FrameReader::new(tokio::io::stdin(), CLIENT_FRAME_CAP);
    let mut output = BufWriter::new(tokio::io::stdout());
    let initialize = match input.read_frame().await {
        Ok(Some(body)) => parse_message(&body).map_err(|error| anyhow!(error))?,
        Ok(None) => return Ok(ExitCode::SUCCESS),
        Err(error) => {
            tracing::error!(%error, "invalid initialize frame");
            return Ok(ExitCode::from(1));
        }
    };
    let initialize_id = initialize.get("id").cloned().unwrap_or(Value::Null);
    let params = initialize
        .get("params")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let settings = match params.get("initializationOptions") {
        Some(options) => match parse_settings(options) {
            Ok(settings) => settings,
            Err(error) => {
                send_error(&mut output, &initialize_id, -32602, "InvalidParams").await?;
                tracing::error!(%error, "invalid initialization settings");
                return Ok(ExitCode::from(1));
            }
        },
        None => Settings::default(),
    };
    let root = match worktree_root_from_initialize(&params) {
        Ok(root) => root,
        Err(error) => {
            send_error(
                &mut output,
                &initialize_id,
                error.lsp_code(),
                &error.to_string(),
            )
            .await?;
            return Ok(ExitCode::from(1));
        }
    };
    let project =
        match find_project_dir(&root, None, settings.project_dir.as_deref().map(Path::new)) {
            Ok(project) => project,
            Err(error) => {
                send_error(
                    &mut output,
                    &initialize_id,
                    error.lsp_code(),
                    &error.to_string(),
                )
                .await?;
                return Ok(ExitCode::from(1));
            }
        };
    let (symbol_sender, symbol_events) = mpsc::unbounded_channel();
    let mut documents = DocumentState::new();
    documents.set_event_hook(Arc::new(move |event| {
        let _ = symbol_sender.send(event);
    }));
    let mut proxy = ProxyState {
        documents,
        pending: HashMap::new(),
        queued: VecDeque::new(),
        server_requests: HashSet::new(),
        stale_server_ids: HashSet::new(),
        next_id: 1,
        initialized_forwarded: false,
        zed_initialized: false,
        initialize: initialize.clone(),
        project: project.clone(),
        recovery_times: VecDeque::new(),
        project_diagnostics_started: false,
        workspace_symbols_notice_sent: false,
        symbol_events,
        symbol_cache: HashMap::new(),
        symbol_scheduled: HashMap::new(),
    };

    if let Some(lsp_port) = settings.lsp_port {
        let dap_port = settings.dap_port;
        let stream = match tokio::time::timeout(
            Duration::from_secs(5),
            TcpStream::connect(("127.0.0.1", lsp_port)),
        )
        .await
        {
            Ok(Ok(stream)) => stream,
            Ok(Err(error)) => {
                send_error(&mut output, &initialize_id, -32002, &error.to_string()).await?;
                return Ok(ExitCode::from(1));
            }
            Err(_) => {
                send_error(
                    &mut output,
                    &initialize_id,
                    -32002,
                    "unmanaged LSP connection timed out",
                )
                .await?;
                return Ok(ExitCode::from(1));
            }
        };
        let connection = connection_from_stream(stream);
        return run_session(
            input,
            output,
            proxy,
            settings,
            Runtime {
                files: ProjectFiles::new(&project)?,
                state: Arc::new(RwLock::new(empty_state(&project, Mode::Unmanaged))),
                socket: None,
                lock: None,
                handoff_receiver: None,
                mode: Mode::Unmanaged,
            },
            Editor {
                child: None,
                connection,
                lsp_port,
                dap_port,
            },
            true,
        )
        .await;
    }

    let files = ProjectFiles::new(&project)?;
    let lock = match try_lock(&files.lock)? {
        Some(lock) => lock,
        None => {
            send_error(
                &mut output,
                &initialize_id,
                -32002,
                &format!(
                    "Another Zed window already serves {}. Godot serves one client at a time.",
                    project.display()
                ),
            )
            .await?;
            return Ok(ExitCode::from(1));
        }
    };
    if let Ok(Some(previous)) = read_state(&files.state) {
        if previous.mode == Mode::Gui && gui_process_alive(&previous) {
            match reconnect_gui(&files, previous, settings.startup_timeout_s).await? {
                GuiReconnect::Ready { state, connection } => {
                    let state = Arc::new(RwLock::new(state));
                    let (handoff_sender, handoff_receiver) = mpsc::unbounded_channel();
                    let socket =
                        serve_owner_socket(&files, Arc::clone(&state), handoff_sender).await?;
                    let mut runtime = Runtime {
                        files,
                        state,
                        socket: Some(socket),
                        lock: Some(lock),
                        handoff_receiver: Some(handoff_receiver),
                        mode: Mode::Gui,
                    };
                    publish(&runtime).await?;
                    let editor = Editor {
                        child: None,
                        connection,
                        lsp_port: runtime
                            .state
                            .read()
                            .await
                            .lsp_port
                            .expect("reconnected GUI has an LSP port"),
                        dap_port: runtime
                            .state
                            .read()
                            .await
                            .dap_port
                            .expect("reconnected GUI has a DAP port"),
                    };
                    match forward_initialize(
                        editor,
                        &mut output,
                        &mut proxy,
                        &project,
                        settings.project_diagnostics,
                    )
                    .await
                    {
                        Ok(editor) => {
                            set_ready(&runtime, &editor).await?;
                            return run_session(
                                input, output, proxy, settings, runtime, editor, false,
                            )
                            .await;
                        }
                        Err(error) => {
                            let code = error.code;
                            let message = error.message;
                            drop(error.editor);
                            cleanup_runtime(&mut runtime, None).await;
                            send_error(&mut output, &initialize_id, code, &message).await?;
                            return Ok(ExitCode::from(1));
                        }
                    }
                }
                GuiReconnect::Dead => cleanup_files(&files),
                GuiReconnect::Deadline { pid, state } => {
                    let _ = write_state(&files.state, &state);
                    drop(lock);
                    crate::state::remove_lock_file(&files.lock);
                    send_error(
                        &mut output,
                        &initialize_id,
                        -32002,
                        &format!("GUI editor {pid} is not answering on its ports"),
                    )
                    .await?;
                    return Ok(ExitCode::from(1));
                }
            }
        } else if previous.mode == Mode::Gui {
            cleanup_files(&files);
        }
    }
    stale_cleanup(&files).await;
    let binary = match resolve_godot(settings.godot_path.as_deref().map(Path::new)) {
        Ok(binary) => binary,
        Err(error) => {
            cleanup_files(&files);
            drop(lock);
            cleanup_lock(&files);
            send_error(&mut output, &initialize_id, -32002, &error).await?;
            return Ok(ExitCode::from(1));
        }
    };
    if let Err(error) = check_version(&binary) {
        cleanup_files(&files);
        drop(lock);
        cleanup_lock(&files);
        send_error(&mut output, &initialize_id, -32002, &error).await?;
        return Ok(ExitCode::from(1));
    }
    let state = Arc::new(RwLock::new(new_state(&project, Mode::Headless)));
    let (handoff_sender, handoff_receiver) = mpsc::unbounded_channel();
    let socket = serve_owner_socket(&files, Arc::clone(&state), handoff_sender).await?;
    let mut runtime = Runtime {
        files,
        state,
        socket: Some(socket),
        lock: Some(lock),
        handoff_receiver: Some(handoff_receiver),
        mode: Mode::Headless,
    };
    publish(&runtime).await?;
    let deadline = startup_deadline(settings.startup_timeout_s);
    let mut editor = None;
    let mut startup_error: Option<StartupError> = None;
    for _ in 0..STARTUP_ATTEMPTS {
        match spawn_one(&binary, &settings, &project, &runtime, deadline).await {
            Ok(candidate) => {
                match forward_initialize(
                    candidate,
                    &mut output,
                    &mut proxy,
                    &project,
                    settings.project_diagnostics,
                )
                .await
                {
                    Ok(candidate) => {
                        proxy.initialized_forwarded = true;
                        editor = Some(candidate);
                        break;
                    }
                    Err(error) => {
                        startup_error = Some(StartupError::Io(error.message));
                        terminate_editor(*error.editor).await;
                    }
                }
            }
            Err(error) => {
                startup_error = Some(error);
                if startup_error
                    .as_ref()
                    .is_some_and(|error| matches!(error, StartupError::Deadline(_)))
                {
                    break;
                }
            }
        }
    }
    let Some(editor) = editor else {
        let message = startup_failure_message(startup_error.as_ref(), settings.startup_timeout_s);
        cleanup_runtime(&mut runtime, None).await;
        send_error(&mut output, &initialize_id, -32002, &message).await?;
        return Ok(ExitCode::from(1));
    };
    let editor = editor;
    let proxy = proxy;
    set_ready(&runtime, &editor).await?;
    let result = run_session(input, output, proxy, settings, runtime, editor, false).await?;
    Ok(result)
}

async fn run_session(
    mut input: FrameReader<tokio::io::Stdin>,
    mut output: ClientWriter,
    mut proxy: ProxyState,
    settings: Settings,
    mut runtime: Runtime,
    mut editor: Editor,
    unmanaged: bool,
) -> Result<ExitCode> {
    if unmanaged {
        let init = proxy.initialize.clone();
        let project = proxy.project.clone();
        match forward_initialize(
            editor,
            &mut output,
            &mut proxy,
            &project,
            settings.project_diagnostics,
        )
        .await
        {
            Ok(new_editor) => {
                editor = new_editor;
                proxy.initialized_forwarded = true;
            }
            Err(error) => {
                let code = error.code;
                let message = error.message;
                drop(error.editor);
                send_error(
                    &mut output,
                    init.get("id").unwrap_or(&Value::Null),
                    code,
                    &message,
                )
                .await?;
                return Ok(ExitCode::from(1));
            }
        }
    }
    let mut handoff_receiver = runtime.handoff_receiver.take();
    let mut watcher = if settings.project_diagnostics {
        Some(
            docs_state::watch_project(&proxy.project)
                .map_err(|error| anyhow!("cannot watch project: {error}"))?,
        )
    } else {
        None
    };
    let mut watcher_pending = Vec::new();
    let mut watcher_deadline = None;
    loop {
        tokio::select! {
            client = input.read_frame() => {
                match client {
                    Ok(Some(body)) => {
                        let message = match parse_message(&body) {
                            Ok(message) => message,
                            Err(error) => {
                                tracing::error!(%error, "malformed client message");
                                cleanup_runtime(&mut runtime, editor.child.take()).await;
                                return Ok(ExitCode::from(1));
                            }
                        };
                        if message.get("method").and_then(Value::as_str) == Some("exit") {
                            cleanup_runtime(&mut runtime, editor.child.take()).await;
                            return Ok(ExitCode::SUCCESS);
                        }
                        if message.get("method").and_then(Value::as_str) == Some("shutdown") {
                            if let Err(error) = forward_client_request(&mut editor, &mut output, &mut proxy, message).await {
                                tracing::error!(%error, "cannot forward shutdown");
                                cleanup_runtime(&mut runtime, editor.child.take()).await;
                                return Ok(ExitCode::from(1));
                            }
                            match editor.connection.reader.read_frame().await {
                                Ok(Some(body)) => {
                                    if let Err(error) = forward_server_message(&mut editor, &mut output, &mut proxy, &body, true).await {
                                        tracing::error!(%error, "cannot forward shutdown response");
                                        cleanup_runtime(&mut runtime, editor.child.take()).await;
                                        return Ok(ExitCode::from(1));
                                    }
                                }
                                Ok(None) | Err(_) => {
                                    cleanup_runtime(&mut runtime, editor.child.take()).await;
                                    return Ok(ExitCode::from(1));
                                }
                            }
                            cleanup_runtime(&mut runtime, editor.child.take()).await;
                            return Ok(ExitCode::SUCCESS);
                        }
                        if let Err(error) = forward_client_message(&mut editor, &mut output, &mut proxy, &settings, message).await {
                            tracing::error!(%error, "cannot forward client message");
                            cleanup_runtime(&mut runtime, editor.child.take()).await;
                            return Ok(ExitCode::from(1));
                        }
                    }
                    Ok(None) => {
                        cleanup_runtime(&mut runtime, editor.child.take()).await;
                        return Ok(ExitCode::SUCCESS);
                    }
                    Err(error) => {
                        tracing::error!(%error, "malformed client frame");
                        cleanup_runtime(&mut runtime, editor.child.take()).await;
                        return Ok(ExitCode::from(1));
                    }
                }
            }
            server = editor.connection.reader.read_frame() => {
                match server {
                    Ok(Some(body)) => {
                        if let Err(error) = forward_server_message(&mut editor, &mut output, &mut proxy, &body, false).await {
                            tracing::error!(%error, "cannot forward server message");
                            if unmanaged {
                                cleanup_runtime(&mut runtime, editor.child.take()).await;
                                return Ok(ExitCode::from(1));
                            }
                            if recover(
                                &mut input,
                                &mut output,
                                &mut proxy,
                                &mut runtime,
                                &settings,
                                &mut editor,
                                &mut watcher,
                                 &mut watcher_pending,
                                 &mut watcher_deadline,
                                 &error.to_string(),
                                 true,
                             ).await.is_err() {
                                cleanup_runtime(&mut runtime, editor.child.take()).await;
                                return Ok(ExitCode::from(1));
                            }
                        }
                    }
                    Ok(None) | Err(_) => {
                        if unmanaged {
                            cleanup_runtime(&mut runtime, editor.child.take()).await;
                            return Ok(ExitCode::from(1));
                        }
                        let reason = editor.child.as_mut().and_then(|child| child.child.try_wait().ok().flatten());
                        if recover_with_status(
                            &mut input,
                            &mut output,
                            &mut proxy,
                            &mut runtime,
                            &settings,
                            &mut editor,
                            &mut watcher,
                            &mut watcher_pending,
                            &mut watcher_deadline,
                            reason,
                        ).await.is_err() {
                            cleanup_runtime(&mut runtime, editor.child.take()).await;
                            return Ok(ExitCode::from(1));
                        }
                     }
                 }
             }
             handoff = next_handoff(&mut handoff_receiver), if handoff_receiver.is_some() => {
                 if let Some(handoff) = handoff {
                     if perform_handoff(
                         &mut input,
                         &mut output,
                         &mut proxy,
                         &mut runtime,
                         &settings,
                         &mut editor,
                         &mut watcher,
                         &mut watcher_pending,
                         &mut watcher_deadline,
                         handoff,
                     ).await.is_err() {
                         cleanup_runtime(&mut runtime, editor.child.take()).await;
                         return Ok(ExitCode::from(1));
                     }
                 } else {
                     handoff_receiver = None;
                 }
             }
             _ = tokio::time::sleep(Duration::from_millis(200)), if runtime.mode == Mode::Gui => {
                 if !gui_process_is_alive(&runtime).await && recover(
                     &mut input,
                     &mut output,
                     &mut proxy,
                     &mut runtime,
                     &settings,
                     &mut editor,
                     &mut watcher,
                     &mut watcher_pending,
                     &mut watcher_deadline,
                     "GUI exited",
                     false,
                 ).await.is_err() {
                     cleanup_runtime(&mut runtime, editor.child.take()).await;
                     return Ok(ExitCode::from(1));
                 }
             }
             watcher_result = next_watcher_event(&mut watcher), if watcher.is_some() => {
                match watcher_result {
                    Some(Ok(event)) => {
                        watcher_pending.extend(docs_state::watcher_changes(event));
                        watcher_deadline = Some(Instant::now() + Duration::from_millis(300));
                    }
                    Some(Err(error)) => tracing::warn!(%error, "project diagnostics watcher error"),
                    None => watcher = None,
                }
            }
            _ = wait_for_watcher_debounce(watcher_deadline), if watcher_deadline.is_some() => {
                let changes = std::mem::take(&mut watcher_pending);
                watcher_deadline = None;
                process_watcher_changes(
                    &mut proxy,
                    &mut output,
                    Some(&mut editor),
                    &settings,
                    changes,
                    false,
                ).await?;
            }
            event = proxy.symbol_events.recv() => {
                if let Some(event) = event {
                    schedule_symbol_event(&mut proxy, event);
                    send_due_symbol_requests(&mut editor, &mut proxy).await?;
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(50)) => {
                send_due_symbol_requests(&mut editor, &mut proxy).await?;
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn recover_with_status(
    input: &mut FrameReader<tokio::io::Stdin>,
    output: &mut ClientWriter,
    proxy: &mut ProxyState,
    runtime: &mut Runtime,
    settings: &Settings,
    editor: &mut Editor,
    watcher: &mut Option<ProjectWatcher>,
    watcher_pending: &mut Vec<WatcherChange>,
    watcher_deadline: &mut Option<Instant>,
    status: Option<ExitStatus>,
) -> Result<()> {
    let code = status.and_then(|status| status.code()).unwrap_or(-1);
    let count_recovery = runtime.mode != Mode::Gui || !gui_process_is_alive(runtime).await;
    recover(
        input,
        output,
        proxy,
        runtime,
        settings,
        editor,
        watcher,
        watcher_pending,
        watcher_deadline,
        &format!("code {code}"),
        count_recovery,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn recover(
    input: &mut FrameReader<tokio::io::Stdin>,
    output: &mut ClientWriter,
    proxy: &mut ProxyState,
    runtime: &mut Runtime,
    settings: &Settings,
    editor: &mut Editor,
    watcher: &mut Option<ProjectWatcher>,
    watcher_pending: &mut Vec<WatcherChange>,
    watcher_deadline: &mut Option<Instant>,
    reason: &str,
    count_recovery: bool,
) -> Result<()> {
    if runtime.mode == Mode::Gui {
        runtime.mode = Mode::Headless;
        runtime.state.write().await.mode = Mode::Headless;
    }
    let now = Instant::now();
    if count_recovery {
        while proxy
            .recovery_times
            .front()
            .is_some_and(|time| now.duration_since(*time) > Duration::from_secs(60))
        {
            proxy.recovery_times.pop_front();
        }
        proxy.recovery_times.push_back(now);
    }
    proxy.documents.set_open_change_events(false);
    proxy.symbol_cache.clear();
    set_recovering(runtime).await?;
    proxy.project_diagnostics_started = false;
    if proxy.recovery_times.len() >= 3 {
        let log_path = runtime.files.state.with_extension("godot.log");
        send_show_message(
            output,
            &format!("Godot keeps crashing, see {}", log_path.display()),
        )
        .await?;
        cleanup_runtime(runtime, editor.child.take()).await;
        return Err(anyhow!("Godot keeps crashing"));
    }
    send_show_message(output, &format!("Godot exited ({reason}), restarting.")).await?;
    for pending in proxy.pending.drain().map(|(_, pending)| pending) {
        if !pending.internal {
            send_error(output, &pending.zed_id, -32803, "RequestFailed").await?;
        }
    }
    for queued in proxy.queued.drain(..) {
        if let Some(id) = queued.get("id") {
            send_error(output, id, -32803, "RequestFailed").await?;
        }
    }
    proxy.stale_server_ids.extend(proxy.server_requests.drain());
    if let Some(child) = editor.child.take() {
        terminate_editor_child(child).await;
    }
    let binary = resolve_godot(settings.godot_path.as_deref().map(Path::new))
        .map_err(|error| anyhow!(error))?;
    let deadline = startup_deadline(settings.startup_timeout_s);
    let mut candidate = None;
    let mut last_error = None;
    let mut recovery_queue = RecoveryQueue::default();
    let mut stdin_closed = false;
    let project = proxy.project.clone();
    for _ in 0..STARTUP_ATTEMPTS {
        let mut spawn = Box::pin(spawn_one(&binary, settings, &project, runtime, deadline));
        loop {
            tokio::select! {
                result = &mut spawn => {
                    match result {
                        Ok(editor) => {
                            candidate = Some(editor);
                            break;
                        }
                        Err(error) => {
                            last_error = Some(error);
                            if last_error.as_ref().is_some_and(|error| matches!(error, StartupError::Deadline(_))) {
                                break;
                            }
                        }
                    }
                    break;
                }
                client = input.read_frame(), if !stdin_closed => {
                    match client {
                        Ok(Some(body)) => queue_recovery_message(&mut recovery_queue, output, &body).await?,
                        Ok(None) => stdin_closed = true,
                        Err(error) => {
                            tracing::error!(%error, "malformed client frame during recovery");
                            return Err(anyhow!("malformed client frame during recovery"));
                        }
                    }
                }
                watcher_result = next_watcher_event(watcher), if watcher.is_some() => {
                    match watcher_result {
                        Some(Ok(event)) => {
                            watcher_pending.extend(docs_state::watcher_changes(event));
                            *watcher_deadline = Some(Instant::now() + Duration::from_millis(300));
                        }
                        Some(Err(error)) => tracing::warn!(%error, "project diagnostics watcher error during recovery"),
                        None => *watcher = None,
                    }
                }
                _ = wait_for_watcher_debounce(*watcher_deadline), if watcher_deadline.is_some() => {
                    let changes = std::mem::take(watcher_pending);
                    *watcher_deadline = None;
                    process_watcher_changes(
                        proxy,
                        output,
                        None,
                        settings,
                        changes,
                        true,
                    ).await?;
                }
            }
        }
        if candidate.is_some()
            || last_error
                .as_ref()
                .is_some_and(|error| matches!(error, StartupError::Deadline(_)))
        {
            break;
        }
    }
    let Some(mut replacement) = candidate else {
        if let Some(error) = last_error {
            send_show_message(output, &error.message()).await?;
        }
        return Err(anyhow!("recovery failed"));
    };
    if !watcher_pending.is_empty() {
        let changes = std::mem::take(watcher_pending);
        *watcher_deadline = None;
        process_watcher_changes(proxy, output, None, settings, changes, true).await?;
    }
    replay_initialize(&mut replacement, proxy).await?;
    *editor = replacement;
    for doc in proxy.documents.open_docs.values_mut() {
        doc.version = 1;
        let message = json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": {"textDocument": {"uri": doc.uri, "languageId": "gdscript", "version": 1, "text": doc.text}}
        });
        send_godot(&mut editor.connection.writer, &message, false).await?;
    }
    let replayed = proxy
        .documents
        .open_docs
        .values()
        .map(|doc| DocumentEvent::Open {
            uri: doc.uri.clone(),
            version: doc.version,
        })
        .collect::<Vec<_>>();
    for event in replayed {
        schedule_symbol_event(proxy, event);
    }
    proxy.documents.set_open_change_events(true);
    start_project_diagnostics(editor, proxy, settings).await?;
    set_ready(runtime, editor).await?;
    while let Some(item) = recovery_queue.items.pop_front() {
        match item {
            RecoveryItem::Request(message) | RecoveryItem::Notification(message) => {
                forward_client_message(editor, output, proxy, settings, message).await?;
            }
            RecoveryItem::Response(message) => {
                let stale = message
                    .get("id")
                    .is_some_and(|id| proxy.stale_server_ids.contains(&id.to_string()));
                if stale {
                    tracing::debug!("dropping response to stale Godot request");
                } else {
                    forward_client_message(editor, output, proxy, settings, message).await?;
                }
            }
        }
    }
    flush_queued(output, editor, proxy).await
}

async fn queue_recovery_message(
    queue: &mut RecoveryQueue,
    output: &mut ClientWriter,
    body: &[u8],
) -> Result<()> {
    let message = parse_message(body).map_err(|error| anyhow!(error))?;
    if message.get("method").is_some() {
        if message.get("id").is_some() {
            if queue.requests >= RECOVERY_QUEUE_CAP {
                if let Some(id) = message.get("id") {
                    send_error(output, id, -32803, "RequestFailed").await?;
                }
                return Ok(());
            }
            queue.requests += 1;
            queue.items.push_back(RecoveryItem::Request(message));
        } else {
            if queue.notifications >= RECOVERY_QUEUE_CAP {
                if let Some(index) = queue
                    .items
                    .iter()
                    .position(|item| matches!(item, RecoveryItem::Notification(_)))
                {
                    queue.items.remove(index);
                    queue.notifications -= 1;
                    tracing::warn!("dropping oldest notification from recovery queue");
                } else {
                    tracing::warn!("dropping notification from full recovery queue");
                    return Ok(());
                }
            }
            queue.notifications += 1;
            queue.items.push_back(RecoveryItem::Notification(message));
        }
    } else if message.get("id").is_some() {
        queue.items.push_back(RecoveryItem::Response(message));
    } else {
        tracing::warn!("dropping invalid message from recovery queue");
    }
    Ok(())
}

async fn replay_initialize(editor: &mut Editor, proxy: &mut ProxyState) -> Result<()> {
    let mut initialize = proxy.initialize.clone();
    let id = proxy.next_id;
    proxy.next_id += 1;
    if let Some(params) = initialize.get_mut("params").and_then(Value::as_object_mut) {
        params.remove("initializationOptions");
    }
    initialize["id"] = json!(id);
    proxy.pending.insert(
        id,
        PendingRequest {
            zed_id: Value::Null,
            internal: true,
            symbol: None,
        },
    );
    send_godot(&mut editor.connection.writer, &initialize, true).await?;
    loop {
        let body = editor
            .connection
            .reader
            .read_frame()
            .await
            .map_err(|error| anyhow!(error.to_string()))?
            .ok_or_else(|| anyhow!("Godot closed during recovery initialize"))?;
        let message = parse_message(&body).map_err(|error| anyhow!(error))?;
        if message.get("method").and_then(Value::as_str) == Some("gdscript_client/changeWorkspace")
        {
            check_workspace(&message, &proxy.project, Some(editor.lsp_port))?;
            if message.get("id").is_some() {
                send_godot(
                    &mut editor.connection.writer,
                    &json!({"jsonrpc":"2.0","id":message["id"],"result":null}),
                    false,
                )
                .await?;
            }
            continue;
        }
        if message.get("id") == Some(&json!(id)) {
            proxy.pending.remove(&id);
            if proxy.zed_initialized {
                send_godot(
                    &mut editor.connection.writer,
                    &json!({"jsonrpc":"2.0","method":"initialized","params":{}}),
                    false,
                )
                .await?;
            }
            return Ok(());
        }
        if message.get("method").is_some() && message.get("id").is_some() {
            send_godot(
                &mut editor.connection.writer,
                &json!({"jsonrpc":"2.0","id":message["id"],"result":null}),
                false,
            )
            .await?;
        }
    }
}

async fn forward_initialize(
    mut editor: Editor,
    output: &mut ClientWriter,
    proxy: &mut ProxyState,
    project: &Path,
    project_diagnostics: bool,
) -> std::result::Result<Editor, InitFailure> {
    let mut initialize = proxy.initialize.clone();
    let original_id = initialize.get("id").cloned().unwrap_or(Value::Null);
    let bridge_id = proxy.next_id;
    proxy.next_id += 1;
    if let Some(params) = initialize.get_mut("params").and_then(Value::as_object_mut) {
        params.remove("initializationOptions");
    }
    initialize["id"] = json!(bridge_id);
    if let Err(error) = send_godot(&mut editor.connection.writer, &initialize, true).await {
        return Err(InitFailure {
            editor: Box::new(editor),
            code: -32002,
            message: error.to_string(),
        });
    }
    loop {
        let frame = match editor.connection.reader.read_frame().await {
            Ok(Some(frame)) => frame,
            Ok(None) => {
                return Err(InitFailure {
                    editor: Box::new(editor),
                    code: -32002,
                    message: "Godot closed during initialize".to_owned(),
                })
            }
            Err(error) => {
                return Err(InitFailure {
                    editor: Box::new(editor),
                    code: -32002,
                    message: error.to_string(),
                })
            }
        };
        let message = match parse_message(&frame) {
            Ok(message) => message,
            Err(error) => {
                return Err(InitFailure {
                    editor: Box::new(editor),
                    code: -32002,
                    message: error,
                })
            }
        };
        if message.get("method").and_then(Value::as_str) == Some("gdscript_client/changeWorkspace")
        {
            if let Err(error) = check_workspace(&message, project, Some(editor.lsp_port)) {
                return Err(InitFailure {
                    editor: Box::new(editor),
                    code: -32002,
                    message: error.to_string(),
                });
            }
            if message.get("id").is_some()
                && send_godot(
                    &mut editor.connection.writer,
                    &json!({"jsonrpc":"2.0","id":message["id"],"result":null}),
                    false,
                )
                .await
                .is_err()
            {
                return Err(InitFailure {
                    editor: Box::new(editor),
                    code: -32002,
                    message: "cannot answer changeWorkspace".to_owned(),
                });
            }
            continue;
        }
        if message.get("id") == Some(&json!(bridge_id)) {
            let mut response = message;
            response["id"] = original_id;
            patch_initialize_response(&mut response);
            if let Err(error) = send_client(output, &response).await {
                return Err(InitFailure {
                    editor: Box::new(editor),
                    code: -32002,
                    message: error.to_string(),
                });
            }
            if !project_diagnostics && !proxy.workspace_symbols_notice_sent {
                if let Err(error) = send_info_message(
                    output,
                    "Project diagnostics are disabled; workspace symbols cover only files open in Zed.",
                )
                .await
                {
                    return Err(InitFailure {
                        editor: Box::new(editor),
                        code: -32002,
                        message: error.to_string(),
                    });
                }
                proxy.workspace_symbols_notice_sent = true;
            }
            proxy.initialized_forwarded = true;
            return Ok(editor);
        }
        if message.get("method").is_some() && message.get("id").is_some() {
            let id = message["id"].to_string();
            proxy.server_requests.insert(id);
            if send_client(output, &message).await.is_err() {
                return Err(InitFailure {
                    editor: Box::new(editor),
                    code: -32002,
                    message: "cannot forward Godot request".to_owned(),
                });
            }
        } else if send_client(output, &message).await.is_err() {
            return Err(InitFailure {
                editor: Box::new(editor),
                code: -32002,
                message: "cannot forward Godot notification".to_owned(),
            });
        }
    }
}

async fn forward_client_message(
    editor: &mut Editor,
    output: &mut ClientWriter,
    proxy: &mut ProxyState,
    settings: &Settings,
    message: Value,
) -> Result<()> {
    let method = message
        .get("method")
        .and_then(Value::as_str)
        .map(str::to_owned);
    if let Some(method) = method.as_deref() {
        if method == "workspace/symbol" {
            let query = message
                .get("params")
                .and_then(|params| params.get("query"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            let result = proxy
                .symbol_cache
                .values()
                .flatten()
                .cloned()
                .collect::<Vec<_>>();
            send_client(output, &json!({"jsonrpc":"2.0","id":message["id"],"result":symbols::search(&result, query).iter().map(symbols::symbol_information).collect::<Vec<_>>() })).await?;
            return Ok(());
        }
        if method == "initialized" {
            send_godot(&mut editor.connection.writer, &message, false).await?;
            proxy.zed_initialized = true;
            start_project_diagnostics(editor, proxy, settings).await?;
            return Ok(());
        }
        if matches!(
            method,
            "textDocument/didOpen" | "textDocument/didChange" | "textDocument/didClose"
        ) {
            for message in
                rewrite_document_messages(proxy, message, method, settings.project_diagnostics)?
            {
                send_godot(&mut editor.connection.writer, &message, false).await?;
            }
            return Ok(());
        }
        if method == "$/cancelRequest" {
            return cancel_request(editor, output, proxy, message).await;
        }
    }
    if message.get("id").is_some() && method.is_none() {
        let body = serde_json::to_vec(&message)?;
        if body.len() > GODOT_WRITE_CAP {
            return Err(anyhow!("Zed response is too large for Godot"));
        }
        let id = message.get("id").map(Value::to_string).unwrap_or_default();
        if proxy.server_requests.remove(&id) {
            editor
                .connection
                .writer
                .write_all(&crate::framing::encode_frame(&body))
                .await?;
        } else if proxy.stale_server_ids.remove(&id) {
            tracing::debug!(%id, "dropping response to stale Godot request");
        }
        return Ok(());
    }
    if method.is_some() && message.get("id").is_some() {
        return forward_client_request(editor, output, proxy, message).await;
    }
    send_godot(&mut editor.connection.writer, &message, false).await
}

async fn forward_client_request(
    editor: &mut Editor,
    output: &mut ClientWriter,
    proxy: &mut ProxyState,
    mut message: Value,
) -> Result<()> {
    let zed_id = message.get("id").cloned().unwrap_or(Value::Null);
    let is_shutdown = message.get("method").and_then(Value::as_str) == Some("shutdown");
    let bridge_id = proxy.next_id;
    proxy.next_id += 1;
    message["id"] = json!(bridge_id);
    let body = serde_json::to_vec(&message)?;
    if body.len() > GODOT_WRITE_CAP {
        send_error(output, &zed_id, -32803, "message too large for Godot").await?;
        return Ok(());
    }
    if proxy.pending.len() >= IN_FLIGHT_CAP && !is_shutdown {
        message["id"] = zed_id;
        proxy.queued.push_back(message);
        return Ok(());
    }
    proxy.pending.insert(
        bridge_id,
        PendingRequest {
            zed_id,
            internal: false,
            symbol: None,
        },
    );
    if let Err(error) = editor
        .connection
        .writer
        .write_all(&crate::framing::encode_frame(&body))
        .await
    {
        proxy.pending.remove(&bridge_id);
        return Err(error.into());
    }
    Ok(())
}

async fn cancel_request(
    editor: &mut Editor,
    output: &mut ClientWriter,
    proxy: &mut ProxyState,
    message: Value,
) -> Result<()> {
    let target = message
        .get("params")
        .and_then(|params| params.get("id"))
        .cloned()
        .unwrap_or(Value::Null);
    if let Some(index) = proxy
        .queued
        .iter()
        .position(|queued| queued.get("id") == Some(&target))
    {
        proxy.queued.remove(index);
        send_error(output, &target, -32800, "RequestCancelled").await?;
        return Ok(());
    }
    if let Some((&bridge_id, _)) = proxy
        .pending
        .iter()
        .find(|(_, pending)| pending.zed_id == target)
    {
        let translated =
            json!({"jsonrpc":"2.0","method":"$/cancelRequest","params":{"id":bridge_id}});
        send_godot(&mut editor.connection.writer, &translated, false).await?;
    }
    Ok(())
}

async fn forward_server_message(
    editor: &mut Editor,
    output: &mut ClientWriter,
    proxy: &mut ProxyState,
    body: &[u8],
    shutdown_response: bool,
) -> Result<()> {
    let message = parse_message(body).map_err(|error| anyhow!(error))?;
    if let Some(method) = message.get("method").and_then(Value::as_str) {
        if method == "gdscript_client/changeWorkspace" {
            check_workspace(&message, &proxy.project, Some(editor.lsp_port))
                .map_err(|error| anyhow!(error.to_string()))?;
            if let Some(id) = message.get("id") {
                send_godot(
                    &mut editor.connection.writer,
                    &json!({"jsonrpc":"2.0","id":id,"result":null}),
                    false,
                )
                .await?;
            }
            return Ok(());
        }
        if let Some(id) = message.get("id") {
            proxy.server_requests.insert(id.to_string());
        }
        send_client(output, &message).await?;
        return Ok(());
    }
    let Some(id) = message.get("id") else {
        send_client(output, &message).await?;
        return Ok(());
    };
    let id_number = id.as_i64().unwrap_or_default();
    if let Some(pending) = proxy.pending.remove(&id_number) {
        if pending.internal {
            if let Some((uri, version)) = pending.symbol {
                let valid = proxy
                    .documents
                    .open_docs
                    .get(&crate::root::doc_key(&uri))
                    .is_some_and(|doc| doc.uri == uri && doc.version == version);
                if valid {
                    proxy.symbol_cache.insert(
                        uri.clone(),
                        symbols::flatten(message.get("result").unwrap_or(&Value::Null), &uri),
                    );
                }
            }
            return Ok(());
        }
        let mut response = message;
        response["id"] = pending.zed_id;
        send_client(output, &response).await?;
        flush_queued(output, editor, proxy).await?;
    } else if !proxy.stale_server_ids.contains(&id.to_string()) && !shutdown_response {
        tracing::debug!(id = %id, "dropping unknown Godot response id");
    }
    Ok(())
}

async fn flush_queued(
    output: &mut ClientWriter,
    editor: &mut Editor,
    proxy: &mut ProxyState,
) -> Result<()> {
    while proxy.pending.len() < IN_FLIGHT_CAP {
        let Some(mut message) = proxy.queued.pop_front() else {
            break;
        };
        let zed_id = message.get("id").cloned().unwrap_or(Value::Null);
        let bridge_id = proxy.next_id;
        proxy.next_id += 1;
        message["id"] = json!(bridge_id);
        let body = serde_json::to_vec(&message)?;
        if body.len() > GODOT_WRITE_CAP {
            send_error(output, &zed_id, -32803, "message too large for Godot").await?;
            continue;
        }
        proxy.pending.insert(
            bridge_id,
            PendingRequest {
                zed_id,
                internal: false,
                symbol: None,
            },
        );
        editor
            .connection
            .writer
            .write_all(&crate::framing::encode_frame(&body))
            .await?;
    }
    Ok(())
}

fn rewrite_document_messages(
    proxy: &mut ProxyState,
    message: Value,
    method: &str,
    reopen_from_disk: bool,
) -> Result<Vec<Value>> {
    let mut message = message;
    let Some(params) = message.get_mut("params").and_then(Value::as_object_mut) else {
        return Ok(vec![message]);
    };
    let Some(uri) = params
        .get("textDocument")
        .and_then(Value::as_object)
        .and_then(|document| document.get("uri"))
        .and_then(Value::as_str)
        .map(str::to_owned)
    else {
        return Ok(vec![message]);
    };
    match method {
        "textDocument/didOpen" => {
            let Some(document) = params.get("textDocument").and_then(Value::as_object) else {
                return Ok(vec![message]);
            };
            let text = document
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let action = proxy.documents.zed_open(&uri, text);
            return Ok(vec![document_action_message(action)]);
        }
        "textDocument/didChange" => {
            let changes = params
                .get("contentChanges")
                .and_then(Value::as_array)
                .cloned();
            let full_sync = changes
                .as_ref()
                .is_some_and(|changes| changes.len() == 1 && changes[0].get("range").is_none());
            if !full_sync {
                tracing::warn!(%uri, "didChange was not a full synchronization");
                return Ok(vec![message]);
            }
            let text = changes
                .as_ref()
                .and_then(|changes| changes[0].get("text"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let Some(action) = proxy.documents.zed_change(&uri, text) else {
                return Ok(vec![message]);
            };
            return Ok(vec![document_action_message(action)]);
        }
        "textDocument/didClose" => {
            let Some(close) = proxy.documents.zed_close(&uri) else {
                return Ok(vec![message]);
            };
            let (key, close_uri) = match &close {
                DocumentAction::Close { key, uri } => (key.clone(), uri.clone()),
                _ => unreachable!(),
            };
            let mut messages = vec![document_action_message(close)];
            if reopen_from_disk && key.is_file() {
                if let Some(text) = docs_state::read_document(&key) {
                    if let Some(open) = proxy.documents.bridge_open_path(&key, text) {
                        messages.push(document_action_message(open));
                    }
                } else {
                    proxy.documents.forget_closed(&key, &close_uri);
                }
            } else {
                proxy.documents.forget_closed(&key, &close_uri);
            }
            return Ok(messages);
        }
        _ => {}
    }
    Ok(vec![message])
}

fn document_action_message(action: DocumentAction) -> Value {
    match action {
        DocumentAction::Open {
            uri, version, text, ..
        } => json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": {"textDocument": {"uri": uri, "languageId": "gdscript", "version": version, "text": text}}
        }),
        DocumentAction::Change {
            uri, version, text, ..
        } => json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didChange",
            "params": {"textDocument": {"uri": uri, "version": version}, "contentChanges": [{"text": text}]}
        }),
        DocumentAction::Close { uri, .. } => json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didClose",
            "params": {"textDocument": {"uri": uri}}
        }),
    }
}

async fn start_project_diagnostics(
    editor: &mut Editor,
    proxy: &mut ProxyState,
    settings: &Settings,
) -> Result<()> {
    if !settings.project_diagnostics
        || !proxy.initialized_forwarded
        || !proxy.zed_initialized
        || proxy.project_diagnostics_started
    {
        return Ok(());
    }
    proxy.project_diagnostics_started = true;
    let documents = docs_state::scan_project(&proxy.project, settings.diagnose_addons);
    let mut opened = 0;
    for document in documents {
        proxy
            .documents
            .register_watcher_path(&document.path, document.key.clone());
        if proxy.documents.open_docs.contains_key(&document.key) {
            continue;
        }
        if let Some(action) = proxy
            .documents
            .bridge_open_path(&document.path, document.text)
        {
            send_godot(
                &mut editor.connection.writer,
                &document_action_message(action),
                false,
            )
            .await?;
            opened += 1;
            if opened % BULK_DOCUMENTS == 0 {
                tokio::time::sleep(Duration::from_millis(BULK_INTERVAL_MS)).await;
            }
        }
    }
    Ok(())
}

async fn process_watcher_changes(
    proxy: &mut ProxyState,
    output: &mut ClientWriter,
    mut editor: Option<&mut Editor>,
    settings: &Settings,
    changes: Vec<WatcherChange>,
    recovering: bool,
) -> Result<()> {
    if !settings.project_diagnostics || !proxy.zed_initialized {
        return Ok(());
    }
    for change in changes {
        let path = docs_state::normalize_path(&change.path);
        match change.kind {
            WatcherChangeKind::Created | WatcherChangeKind::Modified => {
                if !docs_state::eligible_path(&proxy.project, &path, settings.diagnose_addons)
                    || std::fs::symlink_metadata(&path)
                        .ok()
                        .is_some_and(|metadata| metadata.file_type().is_symlink())
                {
                    continue;
                }
                let Some(text) = docs_state::read_document(&path) else {
                    continue;
                };
                let key = docs_state::key_for_path(&path);
                proxy.documents.register_watcher_path(&path, key.clone());
                let action = match proxy.documents.owner(&key) {
                    Some(DocumentOwner::Zed) => None,
                    Some(DocumentOwner::Bridge) => proxy.documents.bridge_change_path(&path, text),
                    None => proxy.documents.bridge_open_path(&path, text),
                };
                if !recovering {
                    if let Some(action) = action {
                        let Some(editor) = editor.as_deref_mut() else {
                            return Err(anyhow!("project diagnostics editor is unavailable"));
                        };
                        send_godot(
                            &mut editor.connection.writer,
                            &document_action_message(action),
                            false,
                        )
                        .await?;
                    }
                }
            }
            WatcherChangeKind::Removed => {
                let Some(key) = proxy.documents.watcher_key(&path) else {
                    continue;
                };
                if proxy.documents.owner(&key) == Some(DocumentOwner::Zed) {
                    continue;
                }
                let Some(action) = proxy.documents.bridge_remove_key(&key) else {
                    continue;
                };
                if !recovering {
                    let Some(editor) = editor.as_deref_mut() else {
                        return Err(anyhow!("project diagnostics editor is unavailable"));
                    };
                    send_godot(
                        &mut editor.connection.writer,
                        &document_action_message(action.clone()),
                        false,
                    )
                    .await?;
                }
                let uri = match action {
                    DocumentAction::Close { uri, .. } => uri,
                    _ => unreachable!(),
                };
                send_client(
                    output,
                    &json!({
                        "jsonrpc": "2.0",
                        "method": "textDocument/publishDiagnostics",
                        "params": {"uri": uri, "diagnostics": []}
                    }),
                )
                .await?;
            }
        }
    }
    Ok(())
}

async fn next_watcher_event(
    watcher: &mut Option<ProjectWatcher>,
) -> Option<notify::Result<notify::Event>> {
    match watcher.as_mut() {
        Some(watcher) => watcher.receiver.recv().await,
        None => std::future::pending().await,
    }
}

async fn wait_for_watcher_debounce(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => {
            tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await;
        }
        None => std::future::pending().await,
    }
}

async fn spawn_one(
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
        Ok(Readiness::ChildExited(status)) => {
            return Err(StartupError::ChildExited(status.code(), child.last_lines()))
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
        Ok(Readiness::ChildExited(status)) => {
            Err(StartupError::ChildExited(status.code(), child.last_lines()))
        }
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

fn connection_from_stream(stream: TcpStream) -> Connection {
    let (read, write) = stream.into_split();
    Connection {
        reader: FrameReader::new(read, GODOT_FRAME_CAP),
        writer: write,
    }
}

fn startup_deadline(seconds: u32) -> Option<Instant> {
    (seconds != 0).then(|| Instant::now() + Duration::from_secs(u64::from(seconds)))
}

async fn terminate_editor(mut editor: Editor) {
    if let Some(child) = editor.child.take() {
        terminate_editor_child(child).await;
    }
}

async fn terminate_editor_child(child: GodotChild) {
    if let Err(error) = kill_group(child).await {
        tracing::warn!(%error, "cannot terminate Godot process group");
    }
}

async fn cleanup_runtime(runtime: &mut Runtime, child: Option<GodotChild>) {
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
            crate::state::remove_lock_file(&runtime.files.lock);
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
        cleanup_lock(&runtime.files);
    }
}

async fn stale_cleanup(files: &ProjectFiles) {
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
                    tracing::warn!(%error, "cannot terminate stale Godot editor");
                }
            }
        }
    }
    let _ = remove_if_stale(files);
    cleanup_files(files);
}

fn cleanup_files(files: &ProjectFiles) {
    let _ = std::fs::remove_file(&files.state);
    let _ = std::fs::remove_file(&files.sock);
}

fn cleanup_lock(files: &ProjectFiles) {
    let _ = std::fs::remove_file(&files.lock);
}

async fn update_state_spawned(
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

async fn set_ready(runtime: &Runtime, editor: &Editor) -> Result<()> {
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

async fn set_recovering(runtime: &Runtime) -> Result<()> {
    runtime.state.write().await.status = Status::Recovering;
    publish(runtime).await
}

async fn publish(runtime: &Runtime) -> Result<()> {
    let state = runtime.state.read().await.clone();
    crate::state::write_state(&runtime.files.state, &state)
        .context("cannot publish bridge state")?;
    Ok(())
}

fn new_state(project: &Path, mode: Mode) -> State {
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

fn empty_state(project: &Path, mode: Mode) -> State {
    new_state(project, mode)
}

fn format_lines(prefix: &str, lines: &[String]) -> String {
    if lines.is_empty() {
        prefix.to_owned()
    } else {
        format!("{prefix}: {}", lines.join(" | "))
    }
}

fn startup_failure_message(error: Option<&StartupError>, seconds: u32) -> String {
    match error {
        Some(StartupError::Deadline(lines)) => format!(
            "Godot did not start within {seconds}s. Last output: {}",
            lines.join(" | ")
        ),
        Some(StartupError::ChildExited(_, lines)) => {
            format!("Godot exited 3 times. Last output: {}", lines.join(" | "))
        }
        Some(error) => error.message(),
        None => "Godot exited 3 times".to_owned(),
    }
}

fn parse_message(body: &[u8]) -> std::result::Result<Value, String> {
    let value: Value =
        serde_json::from_slice(body).map_err(|error| format!("invalid JSON: {error}"))?;
    if !value.is_object() {
        return Err("LSP message is not an object".to_owned());
    }
    Ok(value)
}

async fn send_client<W: AsyncWrite + Unpin>(writer: &mut W, message: &Value) -> Result<()> {
    let body = serde_json::to_vec(message)?;
    if body.len() > CLIENT_FRAME_CAP {
        return Err(anyhow!("Zed output message is oversized"));
    }
    write_frame(writer, &body, CLIENT_FRAME_CAP)
        .await
        .map_err(|error| anyhow!(error.to_string()))?;
    writer.flush().await.context("cannot flush Zed output")?;
    Ok(())
}

async fn send_error<W: AsyncWrite + Unpin>(
    writer: &mut W,
    id: &Value,
    code: i64,
    message: &str,
) -> Result<()> {
    send_client(
        writer,
        &json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}}),
    )
    .await
}

async fn send_show_message<W: AsyncWrite + Unpin>(writer: &mut W, message: &str) -> Result<()> {
    send_message_type(writer, 1, message).await
}

async fn send_info_message<W: AsyncWrite + Unpin>(writer: &mut W, message: &str) -> Result<()> {
    send_message_type(writer, 3, message).await
}

async fn send_message_type<W: AsyncWrite + Unpin>(
    writer: &mut W,
    message_type: i64,
    message: &str,
) -> Result<()> {
    send_client(
        writer,
        &json!({"jsonrpc":"2.0","method":"window/showMessage","params":{"type":message_type,"message":message}}),
    )
    .await
}

async fn send_godot(writer: &mut OwnedWriteHalf, message: &Value, request: bool) -> Result<()> {
    let body = serde_json::to_vec(message)?;
    if body.len() > GODOT_WRITE_CAP {
        if request {
            return Err(anyhow!("message too large for Godot"));
        }
        tracing::warn!(
            size = body.len(),
            "dropping oversized notification to Godot"
        );
        return Ok(());
    }
    writer
        .write_all(&crate::framing::encode_frame(&body))
        .await
        .context("Godot write failed")?;
    Ok(())
}

fn check_workspace(message: &Value, project: &Path, port: Option<u16>) -> Result<()> {
    let path = message
        .get("params")
        .and_then(|params| params.get("path").or_else(|| params.get("workspace")))
        .and_then(Value::as_str);
    let Some(path) = path else { return Ok(()) };
    let actual = PathBuf::from(path)
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(path));
    if actual != project {
        let port = port.map_or_else(|| "unknown".to_owned(), |port| port.to_string());
        return Err(anyhow!(
            "Editor on {port} serves {}, expected {}",
            actual.display(),
            project.display()
        ));
    }
    Ok(())
}

fn schedule_symbol_event(proxy: &mut ProxyState, event: DocumentEvent) {
    match event {
        DocumentEvent::Open { uri, version } | DocumentEvent::Change { uri, version } => {
            proxy.symbol_cache.remove(&uri);
            proxy
                .symbol_scheduled
                .insert(uri, (version, Instant::now() + Duration::from_millis(300)));
        }
        DocumentEvent::Close { uri } | DocumentEvent::Remove { uri } => {
            proxy.symbol_scheduled.remove(&uri);
            proxy.symbol_cache.remove(&uri);
            let key = crate::root::doc_key(&uri);
            proxy.pending.retain(|_, pending| {
                pending
                    .symbol
                    .as_ref()
                    .is_none_or(|(pending_uri, _)| pending_uri != &uri)
                    || proxy.documents.open_docs.contains_key(&key)
            });
        }
    }
}

async fn send_due_symbol_requests(editor: &mut Editor, proxy: &mut ProxyState) -> Result<()> {
    if !proxy.zed_initialized {
        return Ok(());
    }
    let now = Instant::now();
    let due = proxy
        .symbol_scheduled
        .iter()
        .filter(|(_, (_, deadline))| *deadline <= now)
        .map(|(uri, (version, _))| (uri.clone(), *version))
        .collect::<Vec<_>>();
    for (uri, version) in due {
        if proxy.pending.len() >= IN_FLIGHT_CAP {
            break;
        }
        proxy.symbol_scheduled.remove(&uri);
        let id = proxy.next_id;
        proxy.next_id += 1;
        proxy.pending.insert(
            id,
            PendingRequest {
                zed_id: Value::Null,
                internal: true,
                symbol: Some((uri.clone(), version)),
            },
        );
        send_godot(&mut editor.connection.writer, &json!({"jsonrpc":"2.0","id":id,"method":"textDocument/documentSymbol","params":{"textDocument":{"uri":uri}}}), true).await?;
    }
    Ok(())
}

fn patch_initialize_response(response: &mut Value) {
    if let Some(result) = response
        .get_mut("result")
        .and_then(|result| result.get_mut("capabilities"))
        .and_then(Value::as_object_mut)
    {
        result.insert("workspaceSymbolProvider".to_owned(), Value::Bool(true));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_deadline_zero_is_unbounded() {
        assert!(startup_deadline(0).is_none());
    }

    #[test]
    fn document_uri_is_canonicalized() {
        let mut proxy = ProxyState {
            documents: DocumentState::new(),
            pending: HashMap::new(),
            queued: VecDeque::new(),
            server_requests: HashSet::new(),
            stale_server_ids: HashSet::new(),
            next_id: 1,
            initialized_forwarded: false,
            zed_initialized: false,
            initialize: json!({}),
            project: PathBuf::from("/tmp"),
            recovery_times: VecDeque::new(),
            project_diagnostics_started: false,
            workspace_symbols_notice_sent: false,
            symbol_events: mpsc::unbounded_channel().1,
            symbol_cache: HashMap::new(),
            symbol_scheduled: HashMap::new(),
        };
        let message = json!({"jsonrpc":"2.0","method":"textDocument/didOpen","params":{"textDocument":{"uri":"file:///tmp/a.gd","version":42,"text":"x"}}});
        let rewritten =
            rewrite_document_messages(&mut proxy, message, "textDocument/didOpen", true).unwrap();
        assert_eq!(rewritten[0]["params"]["textDocument"]["version"], 1);
        assert_eq!(proxy.documents.open_docs.len(), 1);
    }
}
