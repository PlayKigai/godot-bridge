use crate::error::{Context, Result};
use crate::json::Value;
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{self, BufWriter, Write};
use std::mem::ManuallyDrop;
use std::net::{SocketAddr, TcpStream};
use std::os::fd::FromRawFd;
use std::path::{Path, PathBuf};
use std::process::{ExitCode, ExitStatus};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::{Duration, Instant};

use crate::docs_state::{
    self, DocumentAction, DocumentOwner, DocumentState, WatcherChange, WatcherChangeKind,
    BULK_DOCUMENTS,
};
use crate::framing::{
    parse_json_object, spawn_frame_reader_with, write_frame, write_json, Connection, FrameDecoder,
    ReadEvent,
};
use crate::godot_bin::{check_version, resolve_godot};
use crate::process::{
    kill_group, kill_recorded, pick_free_port, spawn_godot, spawn_gui, wait_for_port, GodotChild,
    Readiness,
};
use crate::root::{find_project_dir, worktree_root_from_initialize};
use crate::settings_file::{parse_trusted_settings, Settings};
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

type ClientWriter = BufWriter<StdoutFile>;

struct StdoutFile(ManuallyDrop<std::fs::File>);

impl Write for StdoutFile {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

struct PendingRequest {
    zed_id: Value,
    internal: bool,
    symbol: Option<(String, u64, i64)>,
}

const SERVER_REQUEST_CAP: usize = 4096;

#[derive(Default)]
struct RequestKeys {
    values: HashSet<crate::json::RequestKey>,
    order: VecDeque<crate::json::RequestKey>,
}

impl RequestKeys {
    fn insert(&mut self, key: crate::json::RequestKey) {
        if self.values.insert(key.clone()) {
            self.order.push_back(key);
            while self.order.len() > SERVER_REQUEST_CAP {
                if let Some(old) = self.order.pop_front() {
                    self.values.remove(&old);
                }
            }
        }
    }

    fn remove(&mut self, key: &crate::json::RequestKey) -> bool {
        self.values.remove(key)
    }

    fn contains(&self, key: &crate::json::RequestKey) -> bool {
        self.values.contains(key)
    }

    fn drain(&mut self) -> Vec<crate::json::RequestKey> {
        self.order.clear();
        self.values.drain().collect()
    }

    fn extend(&mut self, keys: impl IntoIterator<Item = crate::json::RequestKey>) {
        for key in keys {
            self.insert(key);
        }
    }
}

struct ProxyState {
    documents: DocumentState,
    pending: HashMap<i64, PendingRequest>,
    queued: VecDeque<(Value, Value, usize)>,
    queued_bytes: usize,
    server_requests: RequestKeys,
    stale_server_ids: RequestKeys,
    next_id: i64,
    initialized_forwarded: bool,
    zed_initialized: bool,
    initialize: Value,
    project: PathBuf,
    recovery_times: VecDeque<Instant>,
    project_diagnostics_started: bool,
    workspace_symbols_notice_sent: bool,
    internal_sender: mpsc::SyncSender<ProxyEvent>,
    bulk_sender: mpsc::SyncSender<InternalEvent>,
    symbol_cache: HashMap<String, Vec<Symbol>>,
    symbol_containers: symbols::ContainerTable,
    symbol_scheduled: HashMap<String, (u64, i64, Instant, bool)>,
    bulk_generation: u64,
    bulk_documents: VecDeque<docs_state::ScannedDocument>,
    bulk_batch_uris: HashSet<String>,
    bulk_complete: bool,
    bulk_active: bool,
    bulk_replay: bool,
    bulk_deadline: Option<Instant>,
    rescan_generation: u64,
    rescan_keys: HashSet<PathBuf>,
    rescan_active: bool,
}

enum InternalEvent {
    Bulk {
        generation: u64,
        document: docs_state::ScannedDocument,
    },
    BulkComplete {
        generation: u64,
    },
    Rescan {
        generation: u64,
        document: docs_state::ScannedDocument,
    },
    RescanComplete {
        generation: u64,
    },
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
    mode: Mode,
}

#[derive(Default)]
struct Watch {
    watcher: Option<ProjectWatcher>,
    pending: Vec<WatcherChange>,
    deadline: Option<Instant>,
}

struct Session {
    events: Receiver<ProxyEvent>,
    bulk_events: Receiver<InternalEvent>,
    client: FrameState,
    godot: FrameState,
    deferred: DeferredQueue,
    output: ClientWriter,
    proxy: ProxyState,
    runtime: Runtime,
    settings: Settings,
    editor: Editor,
    watch: Watch,
}

enum RecoveryItem {
    Request(Value, usize),
    Notification(Value, usize),
    Response(Value, usize),
}

const MERGED_EVENT_CAP: usize = 64;

enum ProxyEvent {
    Client(ReadEvent),
    Godot(ReadEvent),
    Watcher(std::io::Result<WatcherChange>),
    Handoff(LockGuard),
}

#[derive(Default)]
struct DeferredQueue {
    events: VecDeque<ProxyEvent>,
    bytes: usize,
}

impl DeferredQueue {
    fn push_back(&mut self, event: ProxyEvent) -> Result<()> {
        let size = deferred_event_size(&event);
        if self
            .bytes
            .checked_add(size)
            .is_none_or(|total| total > QUEUE_BYTES_CAP)
        {
            crate::bail!("deferred event queue is too large");
        }
        self.bytes += size;
        self.events.push_back(event);
        Ok(())
    }

    fn pop_front(&mut self) -> Option<ProxyEvent> {
        let event = self.events.pop_front()?;
        self.bytes = self.bytes.saturating_sub(deferred_event_size(&event));
        Some(event)
    }
}

fn deferred_event_size(event: &ProxyEvent) -> usize {
    std::mem::size_of::<ProxyEvent>()
        + match event {
            ProxyEvent::Client(event) | ProxyEvent::Godot(event) => event
                .as_ref()
                .ok()
                .and_then(|event| event.as_ref())
                .map_or(0, Vec::len),
            ProxyEvent::Watcher(Ok(change)) => change.path.as_os_str().len(),
            _ => 0,
        }
}

struct FrameState {
    decoder: FrameDecoder,
    eof: bool,
    frames: VecDeque<Vec<u8>>,
}

impl FrameState {
    fn new(cap: usize) -> Self {
        Self {
            decoder: FrameDecoder::new(cap),
            eof: false,
            frames: VecDeque::new(),
        }
    }

    fn feed(&mut self, event: ReadEvent) -> Result<()> {
        match event {
            Ok(Some(chunk)) => self.decoder.push(&chunk),
            Ok(None) => self.eof = true,
            Err(error) => return Err(error.into()),
        }
        while let Some(body) = self.decoder.next_frame()? {
            self.frames.push_back(body.to_owned());
        }
        if self.eof && self.decoder.has_pending_bytes() {
            return Err(crate::framing::FrameError::Malformed("unexpected EOF".to_owned()).into());
        }
        Ok(())
    }
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

fn reconnect_gui(
    files: &ProjectFiles,
    mut state: State,
    timeout_seconds: u32,
    event_sender: &mpsc::SyncSender<ProxyEvent>,
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
        |sleep_for| {
            thread::sleep(sleep_for);
            Ok(())
        },
    )? {
        DetachedPorts::Ready(stream) => {
            state.status = Status::Ready;
            state.mode = Mode::Gui;
            set_owner_identity(&mut state);
            Ok(GuiReconnect::Ready {
                state,
                connection: connection_from_stream(stream, event_sender.clone()),
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

fn wait_for_detached_ports(
    pid: u32,
    ticks: u64,
    lsp_port: u16,
    dap_port: u16,
    deadline: Option<Instant>,
    mut wait: impl FnMut(Duration) -> Result<()>,
) -> Result<DetachedPorts> {
    loop {
        if !crate::state::pid_alive_with_ticks(pid, ticks) {
            return Ok(DetachedPorts::Dead);
        }
        let lsp_address = SocketAddr::from(([127, 0, 0, 1], lsp_port));
        let dap_address = SocketAddr::from(([127, 0, 0, 1], dap_port));
        if let Ok(lsp) = TcpStream::connect_timeout(&lsp_address, Duration::from_millis(50)) {
            if TcpStream::connect_timeout(&dap_address, Duration::from_millis(50)).is_ok() {
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
        wait(sleep_for)?;
    }
}

fn wait_for_ports_closed(lsp_port: u16, dap_port: u16) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let lsp_closed = TcpStream::connect_timeout(
            &SocketAddr::from(([127, 0, 0, 1], lsp_port)),
            Duration::from_millis(50),
        )
        .is_err();
        let dap_closed = TcpStream::connect_timeout(
            &SocketAddr::from(([127, 0, 0, 1], dap_port)),
            Duration::from_millis(50),
        )
        .is_err();
        if lsp_closed && dap_closed || Instant::now() >= deadline {
            return;
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn gui_process_is_alive(runtime: &Runtime) -> bool {
    let state = runtime
        .state
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    gui_process_alive(&state)
}

fn perform_handoff(session: &mut Session, dap_lock: LockGuard) -> Result<()> {
    let _dap_lock = dap_lock;
    start_recovery(session)?;
    let mut recovery_queue = RecoveryQueue::default();
    let old_lsp_port = session.editor.lsp_port;
    let old_dap_port = session.editor.dap_port;
    let mut gui_identity = None;
    let swap = (|| {
        session.editor.connection.close();
        if let Some(child) = session.editor.child.take() {
            kill_group(child)?;
        }
        wait_for_ports_closed(old_lsp_port, old_dap_port);
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
            let mut state = session
                .runtime
                .state
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state.status = Status::Starting;
            state.mode = Mode::Gui;
            state.godot_pid = Some(pid);
            state.godot_pgid = Some(pgid as u32);
            state.godot_start_ticks = Some(ticks);
            state.lsp_port = Some(old_lsp_port);
            state.dap_port = Some(old_dap_port);
        }
        publish(&session.runtime)?;
        let deadline = startup_deadline(session.settings.startup_timeout_s);
        let lsp_port = session.editor.lsp_port;
        let dap_port = session.editor.dap_port;
        let connection =
            match wait_for_detached_ports(pid, ticks, lsp_port, dap_port, deadline, |sleep_for| {
                match session.events.recv_timeout(sleep_for) {
                    Ok(ProxyEvent::Client(event)) => {
                        session.client.feed(event)?;
                        while let Some(body) = session.client.frames.pop_front() {
                            queue_recovery_message(
                                &mut recovery_queue,
                                &mut session.output,
                                &body,
                            )?;
                        }
                        if session.client.eof && session.client.frames.is_empty() {
                            crate::bail!("Zed closed during GUI handoff");
                        }
                    }
                    Ok(ProxyEvent::Godot(_)) => {}
                    Ok(event) => session.deferred.push_back(event)?,
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        crate::bail!("Zed closed during GUI handoff")
                    }
                }
                Ok(())
            })? {
                DetachedPorts::Ready(stream) => {
                    connection_from_stream(stream, session.proxy.internal_sender.clone())
                }
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
        replay_initialize(
            &mut replacement,
            &mut session.proxy,
            &session.events,
            &mut session.godot,
            &mut session.deferred,
        )?;
        session.editor = replacement;
        finish_recovery(session, &mut recovery_queue)
    })();

    match swap {
        Ok(()) => Ok(()),
        Err(error) => {
            send_show_message(&mut session.output, &error.to_string())?;
            if let Some((pid, pgid, ticks)) = gui_identity {
                let _ = kill_recorded(pid, pgid, ticks);
            }
            fail_recovery_queue(&mut session.output, &mut recovery_queue)?;
            session.runtime.mode = Mode::Headless;
            session
                .runtime
                .state
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .mode = Mode::Headless;
            recover(session, "GUI handoff failed", true)
        }
    }
}

fn fail_recovery_queue(output: &mut ClientWriter, queue: &mut RecoveryQueue) -> Result<()> {
    while let Some(item) = queue.items.pop_front() {
        if let RecoveryItem::Request(message, size) = item {
            queue.bytes = queue.bytes.saturating_sub(size);
            if let Some(id) = message.get("id") {
                send_error(output, id, -32803, "RequestFailed")?;
            }
        }
    }
    Ok(())
}

pub fn run() -> Result<ExitCode> {
    let (event_sender, event_receiver) = mpsc::sync_channel(MERGED_EVENT_CAP);
    let (bulk_sender, bulk_receiver) = mpsc::sync_channel(MERGED_EVENT_CAP);
    let _input_thread = spawn_frame_reader_with(
        "godot-bridge-lsp-client-reader",
        std::io::stdin(),
        event_sender.clone(),
        ProxyEvent::Client,
    )
    .map_err(|error| crate::error::Error::new(error.to_string()))?;
    let mut client = FrameState::new(CLIENT_FRAME_CAP);
    let mut godot = FrameState::new(GODOT_FRAME_CAP);
    let mut deferred = DeferredQueue::default();
    let mut output = client_writer();
    let initialize = match receive_client_frame(&event_receiver, &mut client, &mut deferred)? {
        Some(body) => parse_message(&body).map_err(crate::error::Error::new)?,
        None => return Ok(ExitCode::SUCCESS),
    };
    let initialize_id = initialize.get("id").cloned().unwrap_or(Value::Null);
    let params = initialize
        .get("params")
        .cloned()
        .unwrap_or_else(|| crate::json!({}));
    let root = match worktree_root_from_initialize(&params) {
        Ok(root) => root,
        Err(error) => {
            send_error(
                &mut output,
                &initialize_id,
                error.lsp_code(),
                &error.to_string(),
            )?;
            return Ok(ExitCode::from(1));
        }
    };
    let options = params.get("initializationOptions").unwrap_or(&Value::Null);
    let settings = match parse_trusted_settings(options, &root) {
        Ok(settings) => settings,
        Err(error) => {
            send_error(&mut output, &initialize_id, -32602, "InvalidParams")?;
            crate::error!("invalid initialization settings: {error}");
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
                )?;
                return Ok(ExitCode::from(1));
            }
        };
    let documents = DocumentState::new();
    let internal_sender = event_sender.clone();
    let mut proxy = ProxyState {
        documents,
        pending: HashMap::new(),
        queued: VecDeque::new(),
        queued_bytes: 0,
        server_requests: RequestKeys::default(),
        stale_server_ids: RequestKeys::default(),
        next_id: 1,
        initialized_forwarded: false,
        zed_initialized: false,
        initialize: initialize.clone(),
        project: project.clone(),
        recovery_times: VecDeque::new(),
        project_diagnostics_started: false,
        workspace_symbols_notice_sent: false,
        internal_sender,
        bulk_sender,
        symbol_cache: HashMap::new(),
        symbol_containers: symbols::ContainerTable::default(),
        symbol_scheduled: HashMap::new(),
        bulk_generation: 0,
        bulk_documents: VecDeque::new(),
        bulk_batch_uris: HashSet::new(),
        bulk_complete: false,
        bulk_active: false,
        bulk_replay: false,
        bulk_deadline: None,
        rescan_generation: 0,
        rescan_keys: HashSet::new(),
        rescan_active: false,
    };

    if let Some(lsp_port) = settings.lsp_port {
        let dap_port = settings.dap_port;
        let stream = match TcpStream::connect_timeout(
            &SocketAddr::from(([127, 0, 0, 1], lsp_port)),
            Duration::from_secs(5),
        ) {
            Ok(stream) => stream,
            Err(error) if error.kind() != io::ErrorKind::TimedOut => {
                send_error(&mut output, &initialize_id, -32002, &error.to_string())?;
                return Ok(ExitCode::from(1));
            }
            Err(_) => {
                send_error(
                    &mut output,
                    &initialize_id,
                    -32002,
                    "unmanaged LSP connection timed out",
                )?;
                return Ok(ExitCode::from(1));
            }
        };
        let connection = connection_from_stream(stream, event_sender.clone());
        let session = Session {
            events: event_receiver,
            bulk_events: bulk_receiver,
            client,
            godot,
            deferred,
            output,
            proxy,
            settings,
            runtime: Runtime {
                files: ProjectFiles::new(&project)?,
                state: Arc::new(RwLock::new(new_state(&project, Mode::Unmanaged))),
                socket: None,
                lock: None,
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
        return run_session(session, true);
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
            )?;
            return Ok(ExitCode::from(1));
        }
    };
    if let Ok(Some(previous)) = read_state(&files.state) {
        if previous.mode == Mode::Gui {
            if !matches_project(&previous, &project) {
                drop(lock);
                send_error(&mut output, &initialize_id, -32002, "project mismatch")?;
                return Ok(ExitCode::from(1));
            } else if !gui_process_alive(&previous) {
                cleanup_files(&files);
            } else {
                match reconnect_gui(&files, previous, settings.startup_timeout_s, &event_sender)? {
                    GuiReconnect::Ready { state, connection } => {
                        let state = Arc::new(RwLock::new(state));
                        let socket =
                            serve_owner_socket(&files, Arc::clone(&state), event_sender.clone())?;
                        let mut runtime = Runtime {
                            files,
                            state,
                            socket: Some(socket),
                            lock: Some(lock),
                            mode: Mode::Gui,
                        };
                        publish(&runtime)?;
                        let mut editor = Editor {
                            child: None,
                            connection,
                            lsp_port: runtime
                                .state
                                .read()
                                .unwrap_or_else(|poisoned| poisoned.into_inner())
                                .lsp_port
                                .expect("reconnected GUI has an LSP port"),
                            dap_port: runtime
                                .state
                                .read()
                                .unwrap_or_else(|poisoned| poisoned.into_inner())
                                .dap_port
                                .expect("reconnected GUI has a DAP port"),
                        };
                        if let Err(message) = forward_initialize(
                            &mut editor,
                            &mut output,
                            &mut proxy,
                            &project,
                            settings.project_diagnostics,
                            InitializeInput {
                                events: &event_receiver,
                                godot: &mut godot,
                                deferred: &mut deferred,
                            },
                        ) {
                            drop(editor);
                            cleanup_runtime(&mut runtime, None);
                            send_error(&mut output, &initialize_id, -32002, &message)?;
                            return Ok(ExitCode::from(1));
                        }
                        set_ready(&runtime, &editor)?;
                        return run_session(
                            Session {
                                events: event_receiver,
                                bulk_events: bulk_receiver,
                                client,
                                godot,
                                deferred,
                                output,
                                proxy,
                                settings,
                                runtime,
                                editor,
                                watch: Watch::default(),
                            },
                            false,
                        );
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
                        )?;
                        return Ok(ExitCode::from(1));
                    }
                }
            }
        }
    }
    stale_cleanup(&files, &project);
    let binary = match resolve_godot(settings.godot_path.as_deref().map(Path::new)) {
        Ok(binary) => binary,
        Err(error) => {
            cleanup_files(&files);
            drop(lock);
            send_error(&mut output, &initialize_id, -32002, &error)?;
            return Ok(ExitCode::from(1));
        }
    };
    if let Err(error) = check_version(&binary) {
        cleanup_files(&files);
        drop(lock);
        send_error(&mut output, &initialize_id, -32002, &error)?;
        return Ok(ExitCode::from(1));
    }
    let state = Arc::new(RwLock::new(new_state(&project, Mode::Headless)));
    let socket = serve_owner_socket(&files, Arc::clone(&state), event_sender.clone())?;
    let mut runtime = Runtime {
        files,
        state,
        socket: Some(socket),
        lock: Some(lock),
        mode: Mode::Headless,
    };
    publish(&runtime)?;
    let deadline = startup_deadline(settings.startup_timeout_s);
    let mut editor = None;
    let mut startup_error: Option<StartupError> = None;
    for _ in 0..STARTUP_ATTEMPTS {
        match spawn_one(
            &binary,
            &settings,
            &project,
            &runtime,
            &event_sender,
            deadline,
        ) {
            Ok(mut candidate) => {
                match forward_initialize(
                    &mut candidate,
                    &mut output,
                    &mut proxy,
                    &project,
                    settings.project_diagnostics,
                    InitializeInput {
                        events: &event_receiver,
                        godot: &mut godot,
                        deferred: &mut deferred,
                    },
                ) {
                    Ok(()) => {
                        proxy.initialized_forwarded = true;
                        editor = Some(candidate);
                        break;
                    }
                    Err(message) => {
                        startup_error = Some(StartupError::Io(message));
                        terminate_editor(candidate);
                    }
                }
            }
            Err(error) => {
                startup_error = Some(error);
                let deadline_error = matches!(startup_error, Some(StartupError::Deadline(_)));
                if deadline_error {
                    break;
                }
            }
        }
    }
    let Some(editor) = editor else {
        let message = startup_failure_message(startup_error.as_ref(), settings.startup_timeout_s);
        cleanup_runtime(&mut runtime, None);
        send_error(&mut output, &initialize_id, -32002, &message)?;
        return Ok(ExitCode::from(1));
    };
    set_ready(&runtime, &editor)?;
    run_session(
        Session {
            events: event_receiver,
            bulk_events: bulk_receiver,
            client,
            godot,
            deferred,
            output,
            proxy,
            settings,
            runtime,
            editor,
            watch: Watch::default(),
        },
        false,
    )
}

fn forward_client_body(
    editor: &mut Editor,
    output: &mut ClientWriter,
    proxy: &mut ProxyState,
    settings: &Settings,
    body: &[u8],
    fields: crate::json::TopLevel<'_>,
) -> Result<()> {
    let intercepted = fields.method.is_some_and(|method| {
        method.string_eq("workspace/symbol")
            || method.string_eq("initialized")
            || method.string_eq("textDocument/didOpen")
            || method.string_eq("textDocument/didChange")
            || method.string_eq("textDocument/didClose")
            || method.string_eq("$/cancelRequest")
    });
    if fields.method.is_none() {
        if let Some(raw_id) = fields.id {
            let id = raw_id.request_key();
            if proxy.server_requests.remove(&id) {
                send_godot_body(&mut editor.connection.writer, body, false)?;
            } else if proxy.stale_server_ids.remove(&id) {
                crate::debug!("dropping response to stale Godot request {id:?}");
            }
            return Ok(());
        }
    }
    if fields.id.is_some() || intercepted {
        return forward_client_message(editor, output, proxy, settings, parse_message(body)?);
    }
    send_godot_body(&mut editor.connection.writer, body, false)
}

fn forward_client_message(
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
            let matches = symbols::search_with_uris(
                proxy.symbol_cache.iter().flat_map(|(uri, symbols)| {
                    symbols.iter().map(move |symbol| (uri.as_str(), symbol))
                }),
                query,
                &proxy.symbol_containers,
            );
            let mut body = Vec::new();
            body.extend_from_slice(b"{\"jsonrpc\":\"2.0\",\"id\":");
            body.extend_from_slice(&crate::json::to_vec(&message["id"]));
            body.extend_from_slice(b",\"result\":[");
            for (index, (uri, symbol)) in matches.iter().enumerate() {
                if index != 0 {
                    body.push(b',');
                }
                symbols::write_symbol_information(symbol, uri, &proxy.symbol_containers, &mut body);
            }
            body.extend_from_slice(b"]}");
            send_client_body(output, &body)?;
            return Ok(());
        }
        if method == "initialized" {
            send_godot(&mut editor.connection.writer, &message, false)?;
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
                send_godot_body(&mut editor.connection.writer, &message, false)?;
            }
            return Ok(());
        }
        if method == "$/cancelRequest" {
            return cancel_request(editor, output, proxy, message);
        }
    }
    if message.get("id").is_some() && method.is_none() {
        let body = crate::json::to_vec(&message);
        if body.len() > GODOT_WRITE_CAP {
            crate::bail!("Zed response is too large for Godot");
        }
        let id = message
            .get("id")
            .map(crate::json::value_request_key)
            .expect("response has an id");
        if proxy.server_requests.remove(&id) {
            send_godot_body(&mut editor.connection.writer, &body, false)?;
        } else if proxy.stale_server_ids.remove(&id) {
            crate::debug!("dropping response to stale Godot request {id:?}");
        }
        return Ok(());
    }
    if method.is_some() && message.get("id").is_some() {
        forward_client_request(editor, output, proxy, message)?;
        return Ok(());
    }
    send_godot(&mut editor.connection.writer, &message, false)
}

fn forward_client_request<W: Write>(
    editor: &mut Editor,
    output: &mut W,
    proxy: &mut ProxyState,
    mut message: Value,
) -> Result<Option<i64>> {
    let zed_id = message.get("id").cloned().unwrap_or(Value::Null);
    let is_shutdown = message.get("method").and_then(Value::as_str) == Some("shutdown");
    let bridge_id = proxy.next_id;
    proxy.next_id += 1;
    message["id"] = crate::json!(bridge_id);
    let body = crate::json::to_vec(&message);
    if body.len() > GODOT_WRITE_CAP {
        send_error(output, &zed_id, -32803, "message too large for Godot")?;
        return Ok(None);
    }
    if proxy.pending.len() >= IN_FLIGHT_CAP && !is_shutdown {
        let queued_size = body.len();
        if proxy.queued_bytes.saturating_add(queued_size) > QUEUE_BYTES_CAP {
            send_error(output, &zed_id, -32803, "RequestFailed")?;
            return Ok(None);
        }
        proxy.queued_bytes += queued_size;
        proxy.queued.push_back((message, zed_id, queued_size));
        return Ok(None);
    }
    proxy.pending.insert(
        bridge_id,
        PendingRequest {
            zed_id,
            internal: false,
            symbol: None,
        },
    );
    if let Err(error) = send_godot_body(&mut editor.connection.writer, &body, true) {
        proxy.pending.remove(&bridge_id);
        return Err(error);
    }
    Ok(is_shutdown.then_some(bridge_id))
}

fn cancel_request<W: Write>(
    editor: &mut Editor,
    output: &mut W,
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
        .position(|(_, zed_id, _)| zed_id == &target)
    {
        if let Some((_, _, queued_size)) = proxy.queued.remove(index) {
            proxy.queued_bytes = proxy.queued_bytes.saturating_sub(queued_size);
        }
        send_error(output, &target, -32800, "RequestCancelled")?;
        return Ok(());
    }
    if let Some((&bridge_id, _)) = proxy
        .pending
        .iter()
        .find(|(_, pending)| pending.zed_id == target)
    {
        let translated =
            crate::json!({"jsonrpc":"2.0","method":"$/cancelRequest","params":{"id":bridge_id}});
        send_godot(&mut editor.connection.writer, &translated, false)?;
    }
    Ok(())
}

fn forward_server_message<W: Write>(
    writer: &mut TcpStream,
    lsp_port: u16,
    output: &mut W,
    proxy: &mut ProxyState,
    body: &[u8],
    shutdown_response: bool,
) -> Result<()> {
    let fields = crate::json::scan_top_level(body)?;
    if fields
        .method
        .is_some_and(|method| method.string_eq("textDocument/publishDiagnostics"))
        && fields.id.is_none()
        && proxy.bulk_batch_uris.is_empty()
    {
        send_client_body(output, body)?;
        return Ok(());
    }
    let intercepted = fields.method.is_some_and(|method| {
        method.string_eq("textDocument/publishDiagnostics")
            || method.string_eq("gdscript_client/changeWorkspace")
    });
    if !intercepted && fields.method.is_some() && fields.id.is_none() {
        send_client_body(output, body)?;
        return Ok(());
    }
    let message = parse_message(body)?;
    if let Some(method) = message.get("method").and_then(Value::as_str) {
        if method == "gdscript_client/changeWorkspace" {
            check_workspace(&message, &proxy.project, Some(lsp_port))?;
            if let Some(id) = message.get("id") {
                send_godot(
                    writer,
                    &crate::json!({"jsonrpc":"2.0","id":id,"result":null}),
                    false,
                )?;
            }
            return Ok(());
        }
        if method == "textDocument/publishDiagnostics" && message.get("id").is_none() {
            let uri = message
                .get("params")
                .and_then(|params| params.get("uri"))
                .and_then(Value::as_str);
            if uri.is_some_and(|uri| proxy.bulk_batch_uris.remove(uri))
                && proxy.bulk_batch_uris.is_empty()
            {
                proxy.bulk_deadline = None;
                pump_bulk_documents(proxy, writer)?;
            }
        }
        if message.get("id").is_none() {
            send_client_body(output, body)?;
            return Ok(());
        }
        if let Some(id) = fields.id {
            proxy.server_requests.insert(id.request_key());
        }
        send_client(output, &message)?;
        return Ok(());
    }
    let Some(id) = message.get("id") else {
        send_client(output, &message)?;
        return Ok(());
    };
    let Some(id_number) = fields.id.and_then(|id| id.as_i64()) else {
        if !shutdown_response {
            let stale = fields
                .id
                .is_some_and(|id| proxy.stale_server_ids.contains(&id.request_key()));
            if !stale {
                crate::debug!("dropping unknown Godot response id {id}");
            }
        }
        return Ok(());
    };
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
                    proxy.symbol_cache.remove(&uri);
                    for (symbol_uri, symbol) in symbols::flatten(
                        message.get("result").unwrap_or(&Value::Null),
                        &uri,
                        &mut proxy.symbol_containers,
                    )
                    .into_iter()
                    .filter(|(symbol_uri, _)| symbol_uri == &uri)
                    {
                        proxy
                            .symbol_cache
                            .entry(symbol_uri)
                            .or_default()
                            .push(symbol);
                    }
                }
            }
            flush_queued(output, writer, proxy)?;
            return Ok(());
        }
        let mut response = message;
        response["id"] = pending.zed_id;
        send_client(output, &response)?;
        flush_queued(output, writer, proxy)?;
    } else if !proxy
        .stale_server_ids
        .contains(&fields.id.expect("response has an id").request_key())
        && !shutdown_response
    {
        crate::debug!("dropping unknown Godot response id {id}");
    }
    Ok(())
}

fn flush_queued<W: Write>(
    output: &mut W,
    writer: &mut TcpStream,
    proxy: &mut ProxyState,
) -> Result<()> {
    while proxy.pending.len() < IN_FLIGHT_CAP {
        let Some((mut message, zed_id, queued_size)) = proxy.queued.pop_front() else {
            break;
        };
        proxy.queued_bytes = proxy.queued_bytes.saturating_sub(queued_size);
        let bridge_id = proxy.next_id;
        proxy.next_id += 1;
        message["id"] = crate::json!(bridge_id);
        let body = crate::json::to_vec(&message);
        if body.len() > GODOT_WRITE_CAP {
            send_error(output, &zed_id, -32803, "message too large for Godot")?;
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
        send_godot_body(writer, &body, true)?;
    }
    Ok(())
}

fn rewrite_document_messages(
    proxy: &mut ProxyState,
    message: Value,
    method: &str,
    reopen_from_disk: bool,
) -> Result<Vec<Vec<u8>>> {
    let mut message = message;
    let Some(params) = message.get_mut("params").and_then(Value::as_object_mut) else {
        return Ok(vec![crate::json::to_vec(&message)]);
    };
    let Some(uri) = params
        .get("textDocument")
        .and_then(Value::as_object)
        .and_then(|document| document.get("uri"))
        .and_then(Value::as_str)
        .map(str::to_owned)
    else {
        return Ok(vec![crate::json::to_vec(&message)]);
    };
    match method {
        "textDocument/didOpen" => {
            let Some(document) = params.get("textDocument").and_then(Value::as_object) else {
                return Ok(vec![crate::json::to_vec(&message)]);
            };
            let text = document
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if text.len() > GODOT_WRITE_CAP {
                crate::warn!("skipping oversized didOpen for Godot {uri}");
                return Ok(Vec::new());
            }
            let action = proxy.documents.zed_open(&uri, text.to_owned());
            let body = crate::json::to_vec(&document_action_message(&action));
            if body.len() > GODOT_WRITE_CAP {
                crate::warn!("skipping oversized didOpen for Godot {uri}");
                return Ok(Vec::new());
            }
            schedule_document_action(proxy, &action);
            return Ok(vec![body]);
        }
        "textDocument/didChange" => {
            let changes = params.get("contentChanges").and_then(Value::as_array);
            let full_sync = changes
                .is_some_and(|changes| changes.len() == 1 && changes[0].get("range").is_none());
            if !full_sync {
                crate::warn!("didChange was not a full synchronization {uri}");
                return Ok(vec![crate::json::to_vec(&message)]);
            }
            let text = changes
                .and_then(|changes| changes.first().and_then(|change| change.get("text")))
                .and_then(Value::as_str)
                .unwrap_or_default();
            if text.len() > GODOT_WRITE_CAP {
                crate::warn!("skipping oversized didChange for Godot {uri}");
                return Ok(Vec::new());
            }
            let Some(action) = proxy.documents.zed_change(&uri, text.to_owned()) else {
                let body = crate::json::to_vec(&message);
                if body.len() > GODOT_WRITE_CAP {
                    crate::warn!("skipping oversized didChange for Godot {uri}");
                    return Ok(Vec::new());
                }
                return Ok(vec![body]);
            };
            let body = crate::json::to_vec(&document_action_message(&action));
            if body.len() > GODOT_WRITE_CAP {
                crate::warn!("skipping oversized didChange for Godot {uri}");
                return Ok(Vec::new());
            }
            schedule_document_action(proxy, &action);
            return Ok(vec![body]);
        }
        "textDocument/didClose" => {
            let Some((key, close_uri)) = proxy.documents.zed_close(&uri) else {
                return Ok(vec![crate::json::to_vec(&message)]);
            };
            forget_symbols(proxy, &close_uri);
            let mut messages = vec![crate::json::to_vec(&close_message(&close_uri))];
            if reopen_from_disk && key.is_file() {
                if let Some(text) = docs_state::read_document(&key) {
                    if let Some(open) = proxy.documents.bridge_open_path(&key, text) {
                        schedule_document_action(proxy, &open);
                        messages.push(crate::json::to_vec(&document_action_message(&open)));
                    }
                } else {
                    proxy.documents.forget_closed(&key);
                }
            } else {
                proxy.documents.forget_closed(&key);
            }
            return Ok(messages);
        }
        _ => {}
    }
    Ok(vec![crate::json::to_vec(&message)])
}

fn document_action_message(action: &DocumentAction) -> Value {
    match action {
        DocumentAction::Open {
            uri, version, text, ..
        } => crate::json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": {"textDocument": {"uri": uri, "languageId": "gdscript", "version": (*version), "text": text}}
        }),
        DocumentAction::Change {
            uri, version, text, ..
        } => crate::json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didChange",
            "params": {"textDocument": {"uri": uri, "version": (*version)}, "contentChanges": [{"text": (text)}]}
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
        || proxy.bulk_replay
    {
        return;
    }
    proxy.project_diagnostics_started = true;
    proxy.bulk_generation += 1;
    let generation = proxy.bulk_generation;
    proxy.bulk_documents.clear();
    proxy.bulk_batch_uris.clear();
    proxy.bulk_complete = false;
    proxy.bulk_active = true;
    proxy.bulk_deadline = None;
    let project = proxy.project.clone();
    let diagnose_addons = settings.diagnose_addons;
    let sender = proxy.bulk_sender.clone();
    let _ = thread::Builder::new()
        .name("godot-bridge-project-scan".to_owned())
        .stack_size(256 * 1024)
        .spawn(move || {
            docs_state::scan_project_stream(&project, diagnose_addons, |document| {
                sender
                    .send(InternalEvent::Bulk {
                        generation,
                        document,
                    })
                    .is_ok()
            });
            let _ = sender.send(InternalEvent::BulkComplete { generation });
        });
}

fn start_project_rescan(proxy: &mut ProxyState, settings: &Settings) {
    proxy.rescan_generation += 1;
    let generation = proxy.rescan_generation;
    proxy.rescan_keys.clear();
    proxy.rescan_active = true;
    let project = proxy.project.clone();
    let diagnose_addons = settings.diagnose_addons;
    let sender = proxy.bulk_sender.clone();
    let _ = thread::Builder::new()
        .name("godot-bridge-project-rescan".to_owned())
        .stack_size(256 * 1024)
        .spawn(move || {
            docs_state::scan_project_stream(&project, diagnose_addons, |document| {
                sender
                    .send(InternalEvent::Rescan {
                        generation,
                        document,
                    })
                    .is_ok()
            });
            let _ = sender.send(InternalEvent::RescanComplete { generation });
        });
}

fn process_bulk_document(
    proxy: &mut ProxyState,
    writer: &mut TcpStream,
    document: docs_state::ScannedDocument,
) -> Result<Option<String>> {
    proxy
        .documents
        .register_watcher_path(&document.path, document.key.clone());
    if proxy.bulk_replay {
        let Some(open) = proxy.documents.open_docs.get(&document.key) else {
            return Ok(None);
        };
        let message = crate::json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": {"textDocument": {"uri": (open.uri.clone()), "languageId": "gdscript", "version": 1, "text": (open.text.clone())}}
        });
        send_godot(writer, &message, false)?;
        return Ok(Some(open.uri.clone()));
    }
    if proxy.documents.open_docs.contains_key(&document.key) {
        return Ok(None);
    }
    let action = proxy.documents.bridge_open_key(document.key, document.text);
    if let Some(action) = action {
        let uri = match &action {
            DocumentAction::Open { uri, .. } | DocumentAction::Change { uri, .. } => uri.clone(),
        };
        schedule_document_action(proxy, &action);
        send_godot(writer, &document_action_message(&action), false)?;
        return Ok(Some(uri));
    }
    Ok(None)
}

fn bulk_can_advance(proxy: &ProxyState, now: Instant) -> bool {
    proxy.bulk_batch_uris.is_empty() || proxy.bulk_deadline.is_some_and(|deadline| deadline <= now)
}

fn pump_bulk_documents(proxy: &mut ProxyState, writer: &mut TcpStream) -> Result<()> {
    loop {
        if !bulk_can_advance(proxy, Instant::now()) {
            return Ok(());
        }
        proxy.bulk_batch_uris.clear();
        proxy.bulk_deadline = None;
        let mut sent = 0;
        while sent < BULK_DOCUMENTS {
            let Some(document) = proxy.bulk_documents.pop_front() else {
                break;
            };
            if let Some(uri) = process_bulk_document(proxy, writer, document)? {
                proxy.bulk_batch_uris.insert(uri);
            }
            sent += 1;
        }
        if !proxy.bulk_batch_uris.is_empty() {
            proxy.bulk_deadline = Some(Instant::now() + Duration::from_secs(1));
            return Ok(());
        }
        if proxy.bulk_documents.is_empty() {
            if proxy.bulk_complete {
                proxy.bulk_active = false;
                proxy.bulk_replay = false;
                for (_, _, _, bulk_owned) in proxy.symbol_scheduled.values_mut() {
                    *bulk_owned = false;
                }
            }
            return Ok(());
        }
    }
}

fn process_watcher_changes(
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
        start_project_rescan(proxy, settings);
        changes.retain(|change| change.kind != WatcherChangeKind::Rescan);
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
                            &document_action_message(&action),
                            false,
                        )?;
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
                forget_symbols(proxy, &uri);
                if !recovering {
                    let Some(editor) = editor.as_deref_mut() else {
                        crate::bail!("project diagnostics editor is unavailable");
                    };
                    send_godot(&mut editor.connection.writer, &close_message(&uri), false)?;
                }
                send_client(
                    output,
                    &crate::json!({
                        "jsonrpc": "2.0",
                        "method": "textDocument/publishDiagnostics",
                        "params": {"uri": uri, "diagnostics": []}
                    }),
                )?;
            }
            WatcherChangeKind::Rescan => {}
        }
    }
    Ok(())
}

fn parse_message(body: &[u8]) -> std::result::Result<Value, String> {
    parse_json_object(body, "LSP")
}

fn receive_client_frame(
    events: &Receiver<ProxyEvent>,
    client: &mut FrameState,
    deferred: &mut DeferredQueue,
) -> Result<Option<Vec<u8>>> {
    loop {
        if let Some(body) = client.frames.pop_front() {
            return Ok(Some(body));
        }
        if client.eof {
            return Ok(None);
        }
        let event = events
            .recv()
            .map_err(|_| io::Error::other("event channel is closed"))?;
        match event {
            ProxyEvent::Client(event) => client.feed(event)?,
            event => deferred.push_back(event)?,
        }
    }
}

fn send_client<W: Write>(writer: &mut W, message: &Value) -> Result<()> {
    write_json(writer, message, CLIENT_FRAME_CAP, true)?;
    Ok(())
}

fn client_writer() -> ClientWriter {
    unsafe { BufWriter::new(StdoutFile(ManuallyDrop::new(std::fs::File::from_raw_fd(1)))) }
}

fn send_client_body<W: Write>(writer: &mut W, body: &[u8]) -> Result<()> {
    write_frame(writer, body, CLIENT_FRAME_CAP)?;
    writer.flush()?;
    Ok(())
}

fn send_error<W: Write>(writer: &mut W, id: &Value, code: i64, message: &str) -> Result<()> {
    send_client(
        writer,
        &crate::json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}}),
    )
}

fn send_show_message<W: Write>(writer: &mut W, message: &str) -> Result<()> {
    send_message_type(writer, 1, message)
}

fn send_info_message<W: Write>(writer: &mut W, message: &str) -> Result<()> {
    send_message_type(writer, 3, message)
}

fn send_message_type<W: Write>(writer: &mut W, message_type: i64, message: &str) -> Result<()> {
    send_client(
        writer,
        &crate::json!({"jsonrpc":"2.0","method":"window/showMessage","params":{"type":message_type,"message":message}}),
    )
}

fn send_godot(writer: &mut TcpStream, message: &Value, request: bool) -> Result<()> {
    let body = crate::json::to_vec(message);
    send_godot_body(writer, &body, request)
}

fn send_godot_body(writer: &mut TcpStream, body: &[u8], request: bool) -> Result<()> {
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
    write_frame(writer, body, GODOT_WRITE_CAP).context("Godot write failed")?;
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

fn schedule_symbols(proxy: &mut ProxyState, uri: &str, generation: u64, version: i64) {
    proxy.symbol_cache.remove(uri);
    let bulk_owned = proxy.bulk_active
        && proxy.documents.owner(&proxy.documents.key_for_uri(uri)) == Some(DocumentOwner::Bridge);
    proxy.symbol_scheduled.insert(
        uri.to_owned(),
        (
            generation,
            version,
            Instant::now() + Duration::from_millis(300),
            bulk_owned,
        ),
    );
}

fn forget_symbols(proxy: &mut ProxyState, uri: &str) {
    proxy.symbol_scheduled.remove(uri);
    proxy.symbol_cache.remove(uri);
    proxy.pending.retain(|_, pending| {
        pending
            .symbol
            .as_ref()
            .is_none_or(|(pending_uri, _, _)| pending_uri != uri)
    });
}

fn schedule_document_action(proxy: &mut ProxyState, action: &DocumentAction) {
    match action {
        DocumentAction::Open { uri, version, .. } | DocumentAction::Change { uri, version, .. } => {
            if let Some(generation) = proxy.documents.generation_for_uri(uri) {
                schedule_symbols(proxy, uri, generation, *version);
            }
        }
    }
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

fn next_symbol_deadline(proxy: &ProxyState) -> Option<Instant> {
    if proxy.pending.len() >= IN_FLIGHT_CAP {
        return None;
    }
    proxy
        .symbol_scheduled
        .iter()
        .filter(|(_, (_, _, _, bulk_owned))| !bulk_owned)
        .map(|(_, (_, _, deadline, _))| *deadline)
        .min()
}

fn send_due_symbol_requests(
    editor: &mut Editor,
    proxy: &mut ProxyState,
) -> Result<Option<Instant>> {
    if !proxy.zed_initialized {
        return Ok(None);
    }
    if proxy.pending.len() >= IN_FLIGHT_CAP {
        return Ok(None);
    }
    let now = Instant::now();
    let available = IN_FLIGHT_CAP - proxy.pending.len();
    let due = proxy
        .symbol_scheduled
        .iter()
        .filter(|(_, (_, _, deadline, bulk_owned))| *deadline <= now && !bulk_owned)
        .map(|(uri, (generation, version, _, _))| (uri.clone(), *generation, *version))
        .take(available)
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
        send_godot(
            &mut editor.connection.writer,
            &crate::json!({"jsonrpc":"2.0","id":id,"method":"textDocument/documentSymbol","params":{"textDocument":{"uri":uri}}}),
            true,
        )?;
    }
    if proxy.pending.len() >= IN_FLIGHT_CAP {
        return Ok(None);
    }
    Ok(next_symbol_deadline(proxy))
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
    use std::io::Read;

    #[test]
    fn startup_deadline_zero_is_unbounded() {
        assert!(startup_deadline(0).is_none());
    }

    #[test]
    fn document_uri_is_canonicalized() {
        let internal_sender = mpsc::sync_channel(MERGED_EVENT_CAP).0;
        let bulk_sender = mpsc::sync_channel(MERGED_EVENT_CAP).0;
        let mut proxy = ProxyState {
            documents: DocumentState::new(),
            pending: HashMap::new(),
            queued: VecDeque::new(),
            queued_bytes: 0,
            server_requests: RequestKeys::default(),
            stale_server_ids: RequestKeys::default(),
            next_id: 1,
            initialized_forwarded: false,
            zed_initialized: false,
            initialize: crate::json!({}),
            project: PathBuf::from("/tmp"),
            recovery_times: VecDeque::new(),
            project_diagnostics_started: false,
            workspace_symbols_notice_sent: false,
            internal_sender,
            bulk_sender,
            symbol_cache: HashMap::new(),
            symbol_containers: symbols::ContainerTable::default(),
            symbol_scheduled: HashMap::new(),
            bulk_generation: 0,
            bulk_documents: VecDeque::new(),
            bulk_batch_uris: HashSet::new(),
            bulk_complete: false,
            bulk_active: false,
            bulk_replay: false,
            bulk_deadline: None,
            rescan_generation: 0,
            rescan_keys: HashSet::new(),
            rescan_active: false,
        };
        let message = crate::json!({"jsonrpc":"2.0","method":"textDocument/didOpen","params":{"textDocument":{"uri":"file:///tmp/a.gd","version":42,"text":"x"}}});
        let rewritten =
            rewrite_document_messages(&mut proxy, message, "textDocument/didOpen", true).unwrap();
        let rewritten = crate::json::from_slice(&rewritten[0]).unwrap();
        assert_eq!(rewritten["params"]["textDocument"]["version"], 1);
        assert_eq!(proxy.documents.open_docs.len(), 1);
    }

    #[test]
    fn queued_request_response_keeps_zed_id() {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let writer = TcpStream::connect(address).unwrap();
        let (mut peer, _) = listener.accept().unwrap();
        let socket = writer.try_clone().unwrap();
        let mut editor = Editor {
            child: None,
            connection: Connection::with_parts(socket, None, writer),
            lsp_port: 0,
            dap_port: 0,
        };
        let internal_sender = mpsc::sync_channel(MERGED_EVENT_CAP).0;
        let bulk_sender = mpsc::sync_channel(MERGED_EVENT_CAP).0;
        let mut proxy = ProxyState {
            documents: DocumentState::new(),
            pending: (1..=IN_FLIGHT_CAP as i64)
                .map(|id| {
                    (
                        id,
                        PendingRequest {
                            zed_id: crate::json!(id),
                            internal: false,
                            symbol: None,
                        },
                    )
                })
                .collect(),
            queued: VecDeque::new(),
            queued_bytes: 0,
            server_requests: RequestKeys::default(),
            stale_server_ids: RequestKeys::default(),
            next_id: IN_FLIGHT_CAP as i64 + 1,
            initialized_forwarded: false,
            zed_initialized: false,
            initialize: crate::json!({}),
            project: PathBuf::from("/tmp"),
            recovery_times: VecDeque::new(),
            project_diagnostics_started: false,
            workspace_symbols_notice_sent: false,
            internal_sender,
            bulk_sender,
            symbol_cache: HashMap::new(),
            symbol_containers: symbols::ContainerTable::default(),
            symbol_scheduled: HashMap::new(),
            bulk_generation: 0,
            bulk_documents: VecDeque::new(),
            bulk_batch_uris: HashSet::new(),
            bulk_complete: false,
            bulk_active: false,
            bulk_replay: false,
            bulk_deadline: None,
            rescan_generation: 0,
            rescan_keys: HashSet::new(),
            rescan_active: false,
        };
        let mut output = Vec::new();
        forward_client_request(
            &mut editor,
            &mut output,
            &mut proxy,
            crate::json!({"jsonrpc":"2.0","id":99,"method":"test"}),
        )
        .unwrap();
        proxy.pending.remove(&1);
        flush_queued(&mut output, &mut editor.connection.writer, &mut proxy).unwrap();
        let mut bytes = [0; 1024];
        let size = peer.read(&mut bytes).unwrap();
        let mut decoder = FrameDecoder::new(GODOT_FRAME_CAP);
        decoder.push(&bytes[..size]);
        let request = crate::json::from_slice(decoder.next_frame().unwrap().unwrap()).unwrap();
        let response = format!(
            r#"{{"jsonrpc":"2.0","id":{},"id":{},"result":null}}"#,
            request["id"], request["id"]
        );
        forward_server_message(
            &mut editor.connection.writer,
            0,
            &mut output,
            &mut proxy,
            response.as_bytes(),
            false,
        )
        .unwrap();
        let output = String::from_utf8(output).unwrap();
        let body = output.split_once("\r\n\r\n").unwrap().1;
        assert_eq!(body.matches("\"id\"").count(), 1);
        assert_eq!(crate::json::from_str(body).unwrap()["id"], crate::json!(99));
    }
}
