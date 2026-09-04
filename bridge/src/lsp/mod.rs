use crate::error::{Context, Result};
use crate::json::Value;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::process::{ExitCode, ExitStatus};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncWrite, AsyncWriteExt, BufWriter};
use tokio::net::{tcp::OwnedWriteHalf, TcpStream};
use tokio::sync::mpsc::{self, Receiver, UnboundedReceiver, UnboundedSender};
use tokio::sync::RwLock;

use crate::docs_state::{
    self, DocumentAction, DocumentEvent, DocumentOwner, DocumentState, WatcherChange,
    WatcherChangeKind, BULK_DOCUMENTS, BULK_INTERVAL_MS,
};
use crate::framing::{parse_json_object, write_json, FrameReader};
use crate::godot_bin::{check_version, resolve_godot};
use crate::process::{
    kill_group, kill_recorded, pick_free_port, spawn_godot, spawn_gui, wait_for_port, GodotChild,
    Readiness,
};
use crate::root::{find_project_dir, worktree_root_from_initialize};
use crate::settings_file::{parse_settings, Settings};
use crate::state::{
    clear_owner_identity, gui_process_alive, handoff_decision, matches_project, read_state,
    remove_if_stale, serve_socket, set_owner_identity, start_ticks, try_lock, write_state,
    HandoffDecision, LockGuard, Mode, ProjectFiles, State, Status,
};
use crate::symbols::{self, Symbol};
use crate::watch::ProjectWatcher;

mod handoff;
mod proxy;
mod recovery;
mod startup;

use handoff::*;
use proxy::*;
use recovery::*;
use startup::*;

const CLIENT_FRAME_CAP: usize = 64 * 1024 * 1024;
const GODOT_FRAME_CAP: usize = 64 * 1024 * 1024;
const GODOT_WRITE_CAP: usize = 4 * 1024 * 1024;
const IN_FLIGHT_CAP: usize = 32;
const RECOVERY_QUEUE_CAP: usize = 1000;
const QUEUE_BYTES_CAP: usize = 64 * 1024 * 1024;
const WATCHER_PENDING_CAP: usize = 4096;
const STARTUP_ATTEMPTS: usize = 3;

type ClientWriter = BufWriter<tokio::io::Stdout>;

struct PendingRequest {
    zed_id: Value,
    internal: bool,
    symbol: Option<(String, u64, i64)>,
}

struct ProxyState {
    documents: DocumentState,
    pending: HashMap<i64, PendingRequest>,
    queued: VecDeque<Value>,
    queued_bytes: usize,
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
    internal_events: Receiver<InternalEvent>,
    internal_sender: mpsc::Sender<InternalEvent>,
    symbol_cache: HashMap<String, Vec<Symbol>>,
    symbol_scheduled: HashMap<String, (u64, i64, Instant)>,
    bulk_generation: u64,
}

enum InternalEvent {
    Document(DocumentEvent),
    Bulk {
        generation: u64,
        document: docs_state::ScannedDocument,
    },
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

#[derive(Default)]
struct Watch {
    watcher: Option<ProjectWatcher>,
    pending: Vec<WatcherChange>,
    deadline: Option<Instant>,
}

struct Session {
    input: FrameReader<tokio::io::Stdin>,
    output: ClientWriter,
    proxy: ProxyState,
    runtime: Runtime,
    settings: Settings,
    editor: Editor,
    watch: Watch,
}

enum Recovered {
    Continue,
    ClientClosed,
}

struct HandoffRequest {
    dap_lock: HandoffLock,
}

struct HandoffLock {
    guard: Option<LockGuard>,
}

impl Drop for HandoffLock {
    fn drop(&mut self) {
        self.guard.take();
    }
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
    bytes: usize,
}

enum StartupError {
    ChildExited(Vec<String>),
    Deadline(Vec<String>),
    Io(String),
}

impl StartupError {
    fn message(&self) -> String {
        match self {
            Self::ChildExited(lines) => format_lines("Godot exited", lines),
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

async fn wait_for_detached_ports_during_handoff(
    session: &mut Session,
    pid: u32,
    ticks: u64,
    deadline: Option<Instant>,
    queue: &mut RecoveryQueue,
) -> Result<DetachedPorts> {
    let lsp_port = session.editor.lsp_port;
    let dap_port = session.editor.dap_port;
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
            client = session.input.read_frame() => {
                match client {
                    Ok(Some(body)) => {
                        queue_recovery_message(queue, &mut session.output, &body).await?
                    }
                    Ok(None) => crate::bail!("Zed closed during GUI handoff"),
                    Err(error) => return Err(error.into()),
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

async fn perform_handoff(session: &mut Session, handoff: HandoffRequest) -> Result<Recovered> {
    let _dap_lock = handoff.dap_lock;
    let mut recovery_queue = start_recovery(session).await?;
    let old_lsp_port = session.editor.lsp_port;
    let old_dap_port = session.editor.dap_port;
    let mut gui_identity = None;
    let swap = async {
        if let Some(child) = session.editor.child.take() {
            kill_group(child).await?;
        }
        wait_for_ports_closed(old_lsp_port, old_dap_port).await;
        let binary = resolve_godot(session.settings.godot_path.as_deref().map(Path::new))?;
        check_version(&binary)?;
        let (pid, pgid, ticks) = spawn_gui(
            &binary,
            &session.settings.extra_args,
            &session.proxy.project,
            old_lsp_port,
            old_dap_port,
            session.runtime.files.state.with_extension("gui.log"),
        )?;
        gui_identity = Some((pid, pgid, ticks));
        session.runtime.mode = Mode::Gui;
        {
            let mut state = session.runtime.state.write().await;
            state.status = Status::Starting;
            state.mode = Mode::Gui;
            state.godot_pid = Some(pid);
            state.godot_pgid = Some(pgid as u32);
            state.godot_start_ticks = Some(ticks);
            state.lsp_port = Some(old_lsp_port);
            state.dap_port = Some(old_dap_port);
        }
        publish(&session.runtime).await?;
        let deadline = startup_deadline(session.settings.startup_timeout_s);
        let connection = match wait_for_detached_ports_during_handoff(
            session,
            pid,
            ticks,
            deadline,
            &mut recovery_queue,
        )
        .await?
        {
            DetachedPorts::Ready(stream) => connection_from_stream(stream),
            DetachedPorts::Dead => crate::bail!("GUI editor exited during handoff"),
            DetachedPorts::Deadline => {
                crate::bail!("GUI editor {pid} is not answering on its ports")
            }
        };
        let mut replacement = Editor {
            child: None,
            connection,
            lsp_port: old_lsp_port,
            dap_port: old_dap_port,
        };
        replay_initialize(&mut replacement, &mut session.proxy).await?;
        session.editor = replacement;
        finish_recovery(session, &mut recovery_queue).await
    }
    .await;

    match swap {
        Ok(()) => Ok(Recovered::Continue),
        Err(error) => {
            send_show_message(&mut session.output, &error.to_string()).await?;
            if let Some((pid, pgid, ticks)) = gui_identity {
                let _ = kill_recorded(pid, pgid, ticks).await;
            }
            fail_recovery_queue(&mut session.output, &mut recovery_queue).await?;
            session.runtime.mode = Mode::Headless;
            session.runtime.state.write().await.mode = Mode::Headless;
            recover(session, "GUI handoff failed", true).await
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

pub async fn run() -> Result<ExitCode> {
    let mut input = FrameReader::new(tokio::io::stdin(), CLIENT_FRAME_CAP);
    let mut output = BufWriter::new(tokio::io::stdout());
    let initialize = match input.read_frame().await {
        Ok(Some(body)) => parse_message(&body)?,
        Ok(None) => return Ok(ExitCode::SUCCESS),
        Err(error) => {
            crate::error!("invalid initialize frame: {error}");
            return Ok(ExitCode::from(1));
        }
    };
    let initialize_id = initialize.get("id").cloned().unwrap_or(Value::Null);
    let params = initialize
        .get("params")
        .cloned()
        .unwrap_or_else(|| crate::json!({}));
    let settings = match params.get("initializationOptions") {
        Some(options) => match parse_settings(options) {
            Ok(settings) => settings,
            Err(error) => {
                send_error(&mut output, &initialize_id, -32602, "InvalidParams").await?;
                crate::error!("invalid initialization settings: {error}");
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
    let (internal_sender, internal_events) = mpsc::channel(1024);
    let mut documents = DocumentState::new();
    let event_sender = internal_sender.clone();
    documents.set_event_hook(Arc::new(move |event| {
        let _ = event_sender.try_send(InternalEvent::Document(event));
    }));
    let mut proxy = ProxyState {
        documents,
        pending: HashMap::new(),
        queued: VecDeque::new(),
        queued_bytes: 0,
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
        internal_events,
        internal_sender,
        symbol_cache: HashMap::new(),
        symbol_scheduled: HashMap::new(),
        bulk_generation: 0,
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
        let session = Session {
            input,
            output,
            proxy,
            settings,
            runtime: Runtime {
                files: ProjectFiles::new(&project)?,
                state: Arc::new(RwLock::new(new_state(&project, Mode::Unmanaged))),
                socket: None,
                lock: None,
                handoff_receiver: None,
                mode: Mode::Unmanaged,
            },
            editor: Editor {
                child: None,
                connection,
                lsp_port,
                dap_port,
            },
            watch: Watch::default(),
        };
        return run_session(session, true).await;
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
        if previous.mode == Mode::Gui {
            if !matches_project(&previous, &project) {
                drop(lock);
                send_error(&mut output, &initialize_id, -32002, "project mismatch").await?;
                return Ok(ExitCode::from(1));
            } else if !gui_process_alive(&previous) {
                cleanup_files(&files);
            } else {
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
                        let mut editor = Editor {
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
                        if let Err(message) = forward_initialize(
                            &mut editor,
                            &mut output,
                            &mut proxy,
                            &project,
                            settings.project_diagnostics,
                        )
                        .await
                        {
                            drop(editor);
                            cleanup_runtime(&mut runtime, None).await;
                            send_error(&mut output, &initialize_id, -32002, &message).await?;
                            return Ok(ExitCode::from(1));
                        }
                        set_ready(&runtime, &editor).await?;
                        return run_session(
                            Session {
                                input,
                                output,
                                proxy,
                                settings,
                                runtime,
                                editor,
                                watch: Watch::default(),
                            },
                            false,
                        )
                        .await;
                    }
                    GuiReconnect::Dead => cleanup_files(&files),
                    GuiReconnect::Deadline { pid, state } => {
                        let _ = write_state(&files.state, &state);
                        drop(lock);
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
            }
        }
    }
    stale_cleanup(&files, &project).await;
    let binary = match resolve_godot(settings.godot_path.as_deref().map(Path::new)) {
        Ok(binary) => binary,
        Err(error) => {
            cleanup_files(&files);
            drop(lock);
            send_error(&mut output, &initialize_id, -32002, &error).await?;
            return Ok(ExitCode::from(1));
        }
    };
    if let Err(error) = check_version(&binary) {
        cleanup_files(&files);
        drop(lock);
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
            Ok(mut candidate) => {
                match forward_initialize(
                    &mut candidate,
                    &mut output,
                    &mut proxy,
                    &project,
                    settings.project_diagnostics,
                )
                .await
                {
                    Ok(()) => {
                        proxy.initialized_forwarded = true;
                        editor = Some(candidate);
                        break;
                    }
                    Err(message) => {
                        startup_error = Some(StartupError::Io(message));
                        terminate_editor(candidate).await;
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
    set_ready(&runtime, &editor).await?;
    run_session(
        Session {
            input,
            output,
            proxy,
            settings,
            runtime,
            editor,
            watch: Watch::default(),
        },
        false,
    )
    .await
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
            send_client(output, &crate::json!({"jsonrpc":"2.0","id":(message["id"].clone()),"result":(symbols::search(&result, query).iter().map(symbols::symbol_information).collect::<Vec<_>>()) })).await?;
            return Ok(());
        }
        if method == "initialized" {
            send_godot(&mut editor.connection.writer, &message, false).await?;
            proxy.zed_initialized = true;
            start_project_diagnostics(proxy, settings);
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
        let body = crate::json::to_vec(&message);
        if body.len() > GODOT_WRITE_CAP {
            crate::bail!("Zed response is too large for Godot");
        }
        let id = message.get("id").map(Value::to_string).unwrap_or_default();
        if proxy.server_requests.remove(&id) {
            editor
                .connection
                .writer
                .write_all(&crate::framing::encode_frame(&body))
                .await?;
        } else if proxy.stale_server_ids.remove(&id) {
            crate::debug!("dropping response to stale Godot request {id}");
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
    message["id"] = crate::json!(bridge_id);
    let body = crate::json::to_vec(&message);
    if body.len() > GODOT_WRITE_CAP {
        send_error(output, &zed_id, -32803, "message too large for Godot").await?;
        return Ok(());
    }
    if proxy.pending.len() >= IN_FLIGHT_CAP && !is_shutdown {
        message["id"] = zed_id;
        let queued_size = crate::json::to_vec(&message).len();
        if proxy.queued_bytes.saturating_add(queued_size) > QUEUE_BYTES_CAP {
            if let Some(id) = message.get("id") {
                send_error(output, id, -32803, "RequestFailed").await?;
            }
            return Ok(());
        }
        proxy.queued_bytes += queued_size;
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
        if let Some(queued) = proxy.queued.remove(index) {
            proxy.queued_bytes = proxy
                .queued_bytes
                .saturating_sub(crate::json::to_vec(&queued).len());
        }
        send_error(output, &target, -32800, "RequestCancelled").await?;
        return Ok(());
    }
    if let Some((&bridge_id, _)) = proxy
        .pending
        .iter()
        .find(|(_, pending)| pending.zed_id == target)
    {
        let translated =
            crate::json!({"jsonrpc":"2.0","method":"$/cancelRequest","params":{"id":bridge_id}});
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
    let message = parse_message(body)?;
    if let Some(method) = message.get("method").and_then(Value::as_str) {
        if method == "gdscript_client/changeWorkspace" {
            check_workspace(&message, &proxy.project, Some(editor.lsp_port))?;
            if let Some(id) = message.get("id") {
                send_godot(
                    &mut editor.connection.writer,
                    &crate::json!({"jsonrpc":"2.0","id":id,"result":null}),
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
            if let Some((uri, generation, version)) = pending.symbol {
                let valid = proxy
                    .documents
                    .open_docs
                    .get(&crate::root::doc_key(&uri))
                    .is_some_and(|doc| {
                        doc.uri == uri && doc.generation == generation && doc.version == version
                    });
                if valid {
                    proxy.symbol_cache.insert(
                        uri.clone(),
                        symbols::flatten(message.get("result").unwrap_or(&Value::Null), &uri),
                    );
                }
            }
            flush_queued(output, editor, proxy).await?;
            return Ok(());
        }
        let mut response = message;
        response["id"] = pending.zed_id;
        send_client(output, &response).await?;
        flush_queued(output, editor, proxy).await?;
    } else if !proxy.stale_server_ids.contains(&id.to_string()) && !shutdown_response {
        crate::debug!("dropping unknown Godot response id {id}");
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
        proxy.queued_bytes = proxy
            .queued_bytes
            .saturating_sub(crate::json::to_vec(&message).len());
        let zed_id = message.get("id").cloned().unwrap_or(Value::Null);
        let bridge_id = proxy.next_id;
        proxy.next_id += 1;
        message["id"] = crate::json!(bridge_id);
        let body = crate::json::to_vec(&message);
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
            let planned = proxy.documents.planned_zed_open(&uri, text.clone());
            if crate::json::to_vec(&document_action_message(planned)).len() > GODOT_WRITE_CAP {
                crate::warn!("skipping oversized didOpen for Godot {uri}");
                return Ok(Vec::new());
            }
            let action = proxy.documents.zed_open(&uri, text);
            schedule_document_action(proxy, &action);
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
                crate::warn!("didChange was not a full synchronization {uri}");
                return Ok(vec![message]);
            }
            let text = changes
                .as_ref()
                .and_then(|changes| changes[0].get("text"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let planned = proxy.documents.planned_zed_change(&uri, text.clone());
            if planned.is_some_and(|action| {
                crate::json::to_vec(&document_action_message(action)).len() > GODOT_WRITE_CAP
            }) {
                crate::warn!("skipping oversized didChange for Godot {uri}");
                return Ok(Vec::new());
            }
            let Some(action) = proxy.documents.zed_change(&uri, text) else {
                if crate::json::to_vec(&message).len() > GODOT_WRITE_CAP {
                    crate::warn!("skipping oversized didChange for Godot {uri}");
                    return Ok(Vec::new());
                }
                return Ok(vec![message]);
            };
            schedule_document_action(proxy, &action);
            return Ok(vec![document_action_message(action)]);
        }
        "textDocument/didClose" => {
            let Some((key, close_uri)) = proxy.documents.zed_close(&uri) else {
                return Ok(vec![message]);
            };
            schedule_close(proxy, &close_uri);
            let mut messages = vec![close_message(&close_uri)];
            if reopen_from_disk && key.is_file() {
                if let Some(text) = docs_state::read_document(&key) {
                    if let Some(open) = proxy.documents.bridge_open_path(&key, text) {
                        schedule_document_action(proxy, &open);
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
        } => crate::json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": {"textDocument": {"uri": uri, "languageId": "gdscript", "version": version, "text": text}}
        }),
        DocumentAction::Change {
            uri, version, text, ..
        } => crate::json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didChange",
            "params": {"textDocument": {"uri": uri, "version": version}, "contentChanges": [{"text": (text)}]}
        }),
    }
}

fn close_message(uri: &str) -> Value {
    crate::json!({
        "jsonrpc": "2.0",
        "method": "textDocument/didClose",
        "params": {"textDocument": {"uri": uri}}
    })
}

fn start_project_diagnostics(proxy: &mut ProxyState, settings: &Settings) {
    if !settings.project_diagnostics
        || !proxy.initialized_forwarded
        || !proxy.zed_initialized
        || proxy.project_diagnostics_started
    {
        return;
    }
    proxy.project_diagnostics_started = true;
    proxy.bulk_generation += 1;
    let generation = proxy.bulk_generation;
    let project = proxy.project.clone();
    let diagnose_addons = settings.diagnose_addons;
    let sender = proxy.internal_sender.clone();
    tokio::spawn(async move {
        let documents = tokio::task::spawn_blocking(move || {
            docs_state::scan_project(&project, diagnose_addons)
        })
        .await
        .unwrap_or_default();
        let mut opened = 0;
        for document in documents {
            if sender
                .send(InternalEvent::Bulk {
                    generation,
                    document,
                })
                .await
                .is_err()
            {
                return;
            }
            opened += 1;
            if opened % BULK_DOCUMENTS == 0 {
                tokio::time::sleep(Duration::from_millis(BULK_INTERVAL_MS)).await;
            }
        }
    });
}

async fn process_bulk_document(
    proxy: &mut ProxyState,
    editor: &mut Editor,
    document: docs_state::ScannedDocument,
) -> Result<()> {
    proxy
        .documents
        .register_watcher_path(&document.path, document.key.clone());
    if proxy.documents.open_docs.contains_key(&document.key) {
        return Ok(());
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
    let mut changes = changes;
    if changes
        .iter()
        .any(|change| change.kind == WatcherChangeKind::Rescan)
    {
        let project = proxy.project.clone();
        let diagnose_addons = settings.diagnose_addons;
        let documents = tokio::task::spawn_blocking(move || {
            docs_state::scan_project(&project, diagnose_addons)
        })
        .await
        .unwrap_or_default();
        let scanned_keys = documents
            .iter()
            .map(|document| document.key.clone())
            .collect::<HashSet<_>>();
        changes.retain(|change| change.kind != WatcherChangeKind::Rescan);
        changes.extend(documents.iter().map(|document| WatcherChange {
            kind: if proxy.documents.owner(&document.key).is_some() {
                WatcherChangeKind::Modified
            } else {
                WatcherChangeKind::Created
            },
            path: document.path.clone(),
        }));
        changes.extend(
            proxy
                .documents
                .open_docs
                .iter()
                .filter(|(key, document)| {
                    document.owner == DocumentOwner::Bridge && !scanned_keys.contains(*key)
                })
                .map(|(key, _)| WatcherChange {
                    kind: WatcherChangeKind::Removed,
                    path: key.clone(),
                }),
        );
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
                let key = crate::root::canonical_or_normalized(&path);
                let owner = proxy.documents.owner(&key);
                if owner == Some(DocumentOwner::Zed) {
                    continue;
                }
                let Some(text) = docs_state::read_document(&path) else {
                    continue;
                };
                proxy.documents.register_watcher_path(&path, key.clone());
                let action = match owner {
                    Some(DocumentOwner::Zed) => None,
                    Some(DocumentOwner::Bridge) => proxy.documents.bridge_change_path(&path, text),
                    None => proxy.documents.bridge_open_path(&path, text),
                };
                if let Some(action) = action.as_ref() {
                    schedule_document_action(proxy, action);
                }
                if !recovering {
                    if let Some(action) = action {
                        let Some(editor) = editor.as_deref_mut() else {
                            crate::bail!("project diagnostics editor is unavailable");
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
                let Some(uri) = proxy.documents.bridge_remove_key(&key) else {
                    continue;
                };
                schedule_close(proxy, &uri);
                if !recovering {
                    let Some(editor) = editor.as_deref_mut() else {
                        crate::bail!("project diagnostics editor is unavailable");
                    };
                    send_godot(&mut editor.connection.writer, &close_message(&uri), false).await?;
                }
                send_client(
                    output,
                    &crate::json!({
                        "jsonrpc": "2.0",
                        "method": "textDocument/publishDiagnostics",
                        "params": {"uri": uri, "diagnostics": []}
                    }),
                )
                .await?;
            }
            WatcherChangeKind::Rescan => {}
        }
    }
    Ok(())
}

async fn next_watcher_event(
    watcher: &mut Option<ProjectWatcher>,
) -> Option<std::io::Result<WatcherChange>> {
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

fn parse_message(body: &[u8]) -> std::result::Result<Value, String> {
    parse_json_object(body, "LSP")
}

async fn send_client<W: AsyncWrite + Unpin>(writer: &mut W, message: &Value) -> Result<()> {
    write_json(writer, message, CLIENT_FRAME_CAP, true).await?;
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
        &crate::json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}}),
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
        &crate::json!({"jsonrpc":"2.0","method":"window/showMessage","params":{"type":message_type,"message":message}}),
    )
    .await
}

async fn send_godot(writer: &mut OwnedWriteHalf, message: &Value, request: bool) -> Result<()> {
    let body = crate::json::to_vec(message);
    if body.len() > GODOT_WRITE_CAP {
        if request {
            crate::bail!("message too large for Godot");
        }
        crate::warn!(
            "dropping oversized notification to Godot, {} bytes",
            body.len()
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
        crate::bail!(
            "Editor on {port} serves {}, expected {}",
            actual.display(),
            project.display()
        );
    }
    Ok(())
}

fn schedule_symbol_event(proxy: &mut ProxyState, event: DocumentEvent) {
    match event {
        DocumentEvent::Open {
            uri,
            generation,
            version,
        }
        | DocumentEvent::Change {
            uri,
            generation,
            version,
        } => {
            proxy.symbol_cache.remove(&uri);
            proxy.symbol_scheduled.insert(
                uri,
                (
                    generation,
                    version,
                    Instant::now() + Duration::from_millis(300),
                ),
            );
        }
        DocumentEvent::Close { uri, generation: _ }
        | DocumentEvent::Remove { uri, generation: _ } => {
            proxy.symbol_scheduled.remove(&uri);
            proxy.symbol_cache.remove(&uri);
            proxy.pending.retain(|_, pending| {
                pending
                    .symbol
                    .as_ref()
                    .is_none_or(|(pending_uri, _, _)| pending_uri != &uri)
            });
        }
    }
}

fn schedule_document_action(proxy: &mut ProxyState, action: &DocumentAction) {
    match action {
        DocumentAction::Open { uri, version, .. } | DocumentAction::Change { uri, version, .. } => {
            if let Some(generation) = proxy.documents.generation_for_uri(uri) {
                schedule_symbol_event(
                    proxy,
                    DocumentEvent::Change {
                        uri: uri.clone(),
                        generation,
                        version: *version,
                    },
                );
            }
        }
    }
}

fn schedule_close(proxy: &mut ProxyState, uri: &str) {
    schedule_symbol_event(
        proxy,
        DocumentEvent::Close {
            uri: uri.to_owned(),
            generation: 0,
        },
    );
}

fn coalesce_watcher_changes(changes: Vec<WatcherChange>) -> Vec<WatcherChange> {
    let mut by_path = HashMap::new();
    for change in changes {
        by_path.insert(change.path, change.kind);
    }
    let mut changes = by_path
        .into_iter()
        .map(|(path, kind)| WatcherChange { kind, path })
        .collect::<Vec<_>>();
    changes.sort_by(|left, right| left.path.cmp(&right.path));
    changes
}

async fn send_due_symbol_requests(editor: &mut Editor, proxy: &mut ProxyState) -> Result<()> {
    if !proxy.zed_initialized {
        return Ok(());
    }
    let now = Instant::now();
    let due = proxy
        .symbol_scheduled
        .iter()
        .filter(|(_, (_, _, deadline))| *deadline <= now)
        .map(|(uri, (generation, version, _))| (uri.clone(), *generation, *version))
        .collect::<Vec<_>>();
    for (uri, generation, version) in due {
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
                symbol: Some((uri.clone(), generation, version)),
            },
        );
        send_godot(&mut editor.connection.writer, &crate::json!({"jsonrpc":"2.0","id":id,"method":"textDocument/documentSymbol","params":{"textDocument":{"uri":uri}}}), true).await?;
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
        let (internal_sender, internal_events) = mpsc::channel(1);
        let mut proxy = ProxyState {
            documents: DocumentState::new(),
            pending: HashMap::new(),
            queued: VecDeque::new(),
            queued_bytes: 0,
            server_requests: HashSet::new(),
            stale_server_ids: HashSet::new(),
            next_id: 1,
            initialized_forwarded: false,
            zed_initialized: false,
            initialize: crate::json!({}),
            project: PathBuf::from("/tmp"),
            recovery_times: VecDeque::new(),
            project_diagnostics_started: false,
            workspace_symbols_notice_sent: false,
            internal_events,
            internal_sender,
            symbol_cache: HashMap::new(),
            symbol_scheduled: HashMap::new(),
            bulk_generation: 0,
        };
        let message = crate::json!({"jsonrpc":"2.0","method":"textDocument/didOpen","params":{"textDocument":{"uri":"file:///tmp/a.gd","version":42,"text":"x"}}});
        let rewritten =
            rewrite_document_messages(&mut proxy, message, "textDocument/didOpen", true).unwrap();
        assert_eq!(rewritten[0]["params"]["textDocument"]["version"], 1);
        assert_eq!(proxy.documents.open_docs.len(), 1);
    }
}
