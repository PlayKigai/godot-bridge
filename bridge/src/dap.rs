use crate::error::Result;
use crate::json::Value;
use std::collections::{HashMap, VecDeque};
use std::io::{self, BufWriter, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use crate::exclude::Exclude;
use crate::framing::{
    connect_with_reader, parse_json_object, spawn_frame_reader, write_frame, write_json,
    Connection, FrameDecoder, FrameError, ReadEvent,
};
use crate::root::cwd_root;
use crate::scene::resolve_scene;
use crate::settings_file::{self, Settings};
use crate::state::{socket_request, try_lock, LockGuard, Mode, ProjectFiles};

const FRAME_CAP: usize = 8 * 1024 * 1024;
const BUFFER_CAP: usize = 8 * 1024 * 1024;
const GODOT_WRITE_CAP: usize = 4 * 1024 * 1024;
const SOCKET_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const POLL_INTERVAL: Duration = Duration::from_millis(500);
const PHASE_ONE_GRACE: Duration = Duration::from_secs(10);
const CONSECUTIVE_ERROR_GRACE: Duration = Duration::from_secs(10);
const ATTACH_PRECONDITION: &str = "attach needs a game started from the Godot editor window: run open-editor, then press Play there. Set lsp_port to use your own editor. Games started by run are not attachable; use launch to debug them.";

struct Prepared {
    connection: Connection,
    lock: LockGuard,
    context: DapContext,
}

struct DapContext {
    project: PathBuf,
    file: Option<PathBuf>,
    mode: Mode,
    exclude: Exclude,
}

struct ClientBuffer {
    messages: VecDeque<Vec<u8>>,
    bytes: usize,
}

#[derive(Clone, Copy)]
enum DapSide {
    Client,
    Godot,
}

enum DapReadEvent {
    Client(ReadEvent),
    Godot(ReadEvent),
    Prepared(std::result::Result<Prepared, String>),
}

enum DapFrame {
    Body(DapSide, Vec<u8>),
    End(DapSide),
    Prepared(std::result::Result<Prepared, String>),
}

struct DapInput {
    receiver: Receiver<DapReadEvent>,
    client_decoder: FrameDecoder,
    godot_decoder: FrameDecoder,
    client_eof: bool,
    godot_eof: bool,
    pending: VecDeque<DapFrame>,
}

impl DapInput {
    fn new(receiver: Receiver<DapReadEvent>) -> Self {
        Self {
            receiver,
            client_decoder: FrameDecoder::new(FRAME_CAP),
            godot_decoder: FrameDecoder::new(FRAME_CAP),
            client_eof: false,
            godot_eof: false,
            pending: VecDeque::new(),
        }
    }

    fn defer(&mut self, frame: DapFrame) {
        self.pending.push_back(frame);
    }

    fn recv_frame(&mut self) -> std::result::Result<DapFrame, FrameError> {
        loop {
            if let Some(frame) = self.pending.pop_front() {
                return Ok(frame);
            }
            if let Some(body) = self.client_decoder.next_frame()? {
                return Ok(DapFrame::Body(DapSide::Client, body.to_owned()));
            }
            if let Some(body) = self.godot_decoder.next_frame()? {
                return Ok(DapFrame::Body(DapSide::Godot, body.to_owned()));
            }
            if self.client_eof {
                return Ok(DapFrame::End(DapSide::Client));
            }
            if self.godot_eof {
                return Ok(DapFrame::End(DapSide::Godot));
            }
            match self.receiver.recv() {
                Ok(DapReadEvent::Client(Ok(Some(chunk)))) => self.client_decoder.push(&chunk),
                Ok(DapReadEvent::Godot(Ok(Some(chunk)))) => self.godot_decoder.push(&chunk),
                Ok(DapReadEvent::Client(Ok(None))) => self.client_eof = true,
                Ok(DapReadEvent::Godot(Ok(None))) => self.godot_eof = true,
                Ok(DapReadEvent::Client(Err(error))) | Ok(DapReadEvent::Godot(Err(error))) => {
                    return Err(FrameError::Io(error));
                }
                Ok(DapReadEvent::Prepared(result)) => return Ok(DapFrame::Prepared(result)),
                Err(_) => return Err(FrameError::Io(io::Error::other("reader is closed"))),
            }
        }
    }
}

impl ClientBuffer {
    fn new() -> Self {
        Self {
            messages: VecDeque::new(),
            bytes: 0,
        }
    }

    fn push(&mut self, body: &[u8]) -> std::result::Result<(), ()> {
        let size = body.len();
        if self
            .bytes
            .checked_add(size)
            .is_none_or(|total| total > BUFFER_CAP)
        {
            return Err(());
        }
        crate::json::scan_top_level(body).map_err(|_| ())?;
        self.bytes += size;
        self.messages.push_back(body.to_owned());
        Ok(())
    }

    fn pop(&mut self) -> Option<Vec<u8>> {
        let message = self.messages.pop_front()?;
        self.bytes = self.bytes.saturating_sub(message.len());
        Some(message)
    }
}

struct ClientOutput<W: Write> {
    writer: BufWriter<W>,
    next_seq: i64,
}

impl<W: Write> ClientOutput<W> {
    fn new(writer: W) -> Self {
        Self {
            writer: BufWriter::new(writer),
            next_seq: 1,
        }
    }

    fn send(&mut self, mut message: Value) -> Result<()> {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.saturating_add(1);
        message["seq"] = crate::json!(seq);
        write_json(&mut self.writer, &message, FRAME_CAP, true)?;
        Ok(())
    }

    fn failure(&mut self, request_seq: Value, command: &str, message: &str) -> Result<()> {
        self.send(crate::json!({
            "type": "response",
            "request_seq": (request_seq),
            "command": (command),
            "success": false,
            "message": message,
        }))
    }
}

struct ServerRequests {
    original_sequences: HashMap<crate::json::RequestKey, Value>,
    keys: crate::lsp::RequestKeys,
}

impl ServerRequests {
    fn new() -> Self {
        Self {
            original_sequences: HashMap::new(),
            keys: crate::lsp::RequestKeys::default(),
        }
    }

    fn rewrite(&mut self, message: &mut Value, bridge_seq: i64) {
        if message.get("type").and_then(Value::as_str) == Some("request") {
            if let Some(original) = message.get("seq").cloned() {
                let key = crate::json::RequestKey::Number(bridge_seq);
                if self
                    .original_sequences
                    .insert(key.clone(), original)
                    .is_none()
                {
                    self.keys.insert(key);
                    self.original_sequences
                        .retain(|key, _| self.keys.contains(key));
                }
            }
        }
    }

    fn restore_response(&mut self, message: &mut Value) {
        if message.get("type").and_then(Value::as_str) != Some("response") {
            return;
        }
        let Some(request_seq) = message.get("request_seq") else {
            return;
        };
        if let Some(key) = crate::json::value_request_key(request_seq) {
            if let Some(original) = self.original_sequences.remove(&key) {
                self.keys.remove(&key);
                message["request_seq"] = original;
            }
        }
    }
}

struct RequestFailure {
    request_seq: Value,
    command: String,
    message: String,
}

enum InitializeWait {
    Ready,
    ClientEof,
    ClientInvalid,
    GodotDead,
}

pub fn run(file: Option<PathBuf>) -> crate::error::Result<ExitCode> {
    let (sender, receiver) = mpsc::sync_channel(1);
    let _input_thread = spawn_frame_reader(
        "godot-bridge-dap-client-reader",
        std::io::stdin(),
        sender.clone(),
        DapReadEvent::Client,
    )?;
    let mut input = DapInput::new(receiver);
    let mut output = ClientOutput::new(std::io::stdout());
    let initialize = loop {
        match input.recv_frame()? {
            DapFrame::Body(DapSide::Client, body) => {
                let Ok(message) = parse_message(&body) else {
                    return Ok(ExitCode::from(1));
                };
                let is_request = message.get("type").and_then(Value::as_str) == Some("request");
                if is_request
                    && message.get("command").and_then(Value::as_str) == Some("initialize")
                {
                    break message;
                }
                if is_request {
                    output.failure(
                        message.get("seq").cloned().unwrap_or(Value::Null),
                        message
                            .get("command")
                            .and_then(Value::as_str)
                            .unwrap_or_default(),
                        "first request must be initialize",
                    )?;
                }
                return Ok(ExitCode::from(1));
            }
            DapFrame::Body(side, body) => input.defer(DapFrame::Body(side, body)),
            DapFrame::End(DapSide::Client) => return Ok(ExitCode::SUCCESS),
            DapFrame::End(DapSide::Godot) => return Ok(ExitCode::from(1)),
            DapFrame::Prepared(_) => unreachable!(),
        }
    };

    let mut buffer = ClientBuffer::new();
    let mut early_frames = VecDeque::new();
    let mut early_bytes = 0;
    let cancel = Arc::new(AtomicBool::new(false));
    let cancel_for_worker = Arc::clone(&cancel);
    let prepared_sender = sender.clone();
    let reader_sender = sender.clone();
    let worker = thread::Builder::new()
        .name("godot-bridge-dap-prepare".to_owned())
        .stack_size(256 * 1024)
        .spawn(move || {
            let result = prepare(file.as_deref(), &cancel_for_worker, reader_sender);
            let _ = prepared_sender.send(DapReadEvent::Prepared(result));
        })?;
    let prepared = loop {
        let frame = match input.recv_frame() {
            Ok(frame) => frame,
            Err(error) => {
                cancel.store(true, Ordering::Release);
                drop(input);
                let _ = worker.join();
                return Err(error.into());
            }
        };
        match frame {
            DapFrame::Body(DapSide::Client, body) => {
                if buffer.push(&body).is_err() {
                    cancel.store(true, Ordering::Release);
                    drop(input);
                    let _ = worker.join();
                    return Ok(ExitCode::from(1));
                }
            }
            DapFrame::Body(side, body) => {
                if body.len() > BUFFER_CAP {
                    cancel.store(true, Ordering::Release);
                    drop(input);
                    let _ = worker.join();
                    return Ok(ExitCode::from(1));
                }
                early_bytes += body.len();
                early_frames.push_back(DapFrame::Body(side, body));
                while early_bytes > BUFFER_CAP {
                    let Some(frame) = early_frames.pop_front() else {
                        break;
                    };
                    if let DapFrame::Body(_, body) = frame {
                        early_bytes -= body.len();
                    }
                }
            }
            DapFrame::End(side) => {
                early_frames.push_back(DapFrame::End(side));
                if matches!(side, DapSide::Client) {
                    cancel.store(true, Ordering::Release);
                    drop(input);
                    let _ = worker.join();
                    return Ok(ExitCode::from(1));
                }
            }
            DapFrame::Prepared(result) => break result,
        }
    };
    let mut prepared = match prepared {
        Ok(prepared) => prepared,
        Err(message) => {
            cancel.store(true, Ordering::Release);
            output.failure(
                initialize.get("seq").cloned().unwrap_or(Value::Null),
                "initialize",
                &message,
            )?;
            drop(input);
            let _ = worker.join();
            return Ok(ExitCode::from(1));
        }
    };
    for frame in early_frames {
        input.defer(frame);
    }
    let result = run_session_inner(
        &initialize,
        &mut prepared.connection,
        &mut output,
        &mut input,
        &mut buffer,
        &prepared.context,
    );
    drop(prepared.lock);
    let _ = worker.join();
    result
}

fn prepare(
    file: Option<&Path>,
    cancel: &AtomicBool,
    sender: mpsc::SyncSender<DapReadEvent>,
) -> std::result::Result<Prepared, String> {
    if cancel.load(Ordering::Acquire) {
        return Err("DAP startup cancelled".to_owned());
    }
    let root = cwd_root().map_err(|error| error.to_string())?;
    let settings = settings_file::load_cli(&root)?;
    let (project, file) = crate::root::resolve_project_and_file(
        &root,
        file,
        settings.project_dir.as_deref().map(Path::new),
    )
    .map_err(|error| error.to_string())?;
    let files = ProjectFiles::new(&project).map_err(|error| error.to_string())?;

    let (dap_port, mode) = if settings.lsp_port.is_some() {
        (settings.dap_port, Mode::Unmanaged)
    } else {
        discover_owner(&files, &project, &settings, cancel)?
    };
    if cancel.load(Ordering::Acquire) {
        return Err("DAP startup cancelled".to_owned());
    }
    let lock = match try_lock(&files.dap_lock).map_err(|error| error.to_string())? {
        Some(guard) => guard,
        None => {
            return Err(format!(
                "A debug session for {} is already running (or an editor hand-off is in progress)",
                project.display()
            ));
        }
    };
    let (dap_port, mode) = match settings.lsp_port {
        Some(_) => (dap_port, mode),
        None => refresh_owner_status(&files, &project)?,
    };
    if cancel.load(Ordering::Acquire) {
        return Err("DAP startup cancelled".to_owned());
    }
    let stream = connect_dap(dap_port)?;
    let connection = connect_with_reader(
        stream,
        "godot-bridge-dap-reader",
        sender,
        DapReadEvent::Godot,
    )
    .map_err(|error| error.to_string())?;
    Ok(Prepared {
        connection,
        lock,
        context: DapContext {
            project,
            file,
            mode,
            exclude: Exclude::new(&settings.exclude),
        },
    })
}

fn connect_dap(port: u16) -> std::result::Result<TcpStream, String> {
    let address = SocketAddr::from(([127, 0, 0, 1], port));
    match TcpStream::connect_timeout(&address, SOCKET_REQUEST_TIMEOUT) {
        Ok(stream) => Ok(stream),
        Err(error) if error.kind() == io::ErrorKind::TimedOut => Err(format!(
            "cannot connect to Godot DAP on port {port}: timed out"
        )),
        Err(error) => Err(format!(
            "cannot connect to Godot DAP on port {port}: {error}"
        )),
    }
}

fn status_request(project: &Path) -> Value {
    crate::json!({"cmd": "status", "project": (project.to_string_lossy().into_owned())})
}

fn no_owner_message(project: &Path) -> String {
    format!(
        "No Godot language server runs for {}. Open a .gd file of the project in your editor first.",
        project.display()
    )
}

fn stale_owner_message(project: &Path) -> String {
    format!(
        "A previous language server for {} did not exit cleanly; restart it (:GodotRestart / Godot: Restart Language Server)",
        project.display()
    )
}

fn owner_exited_message(project: &Path) -> String {
    format!(
        "Godot language server for {} exited while starting; check :GodotLog / the Output panel",
        project.display()
    )
}

fn mismatch_message(project: &Path) -> String {
    format!(
        "Godot language server for {} answered \"project mismatch\"; restart it (:GodotRestart / Godot: Restart Language Server)",
        project.display()
    )
}

fn unexpected_reply_message(project: &Path) -> String {
    format!(
        "unexpected owner reply for {}; restart it (:GodotRestart / Godot: Restart Language Server)",
        project.display()
    )
}

fn owner_error_message(error: &io::Error, project: &Path) -> Option<String> {
    match error.kind() {
        io::ErrorKind::PermissionDenied => Some(format!(
            "cannot talk to the Godot language server for {}: {error}",
            project.display()
        )),
        io::ErrorKind::Other => Some(unexpected_reply_message(project)),
        _ => None,
    }
}

#[derive(Clone, Copy)]
enum OwnerStatus {
    Ready { port: u16, mode: Mode },
    Busy,
    Mismatch,
    Unexpected,
}

fn owner_status(reply: &Value) -> OwnerStatus {
    if reply.get("error").and_then(Value::as_str) == Some("project mismatch") {
        return OwnerStatus::Mismatch;
    }
    match reply.get("status").and_then(Value::as_str) {
        Some("ready") => {
            let Some(port) = reply
                .get("dap_port")
                .and_then(Value::as_u64)
                .and_then(|port| u16::try_from(port).ok())
            else {
                return OwnerStatus::Unexpected;
            };
            OwnerStatus::Ready {
                port,
                mode: match reply.get("mode").and_then(Value::as_str) {
                    Some("headless") => Mode::Headless,
                    Some("gui") => Mode::Gui,
                    _ => Mode::Unmanaged,
                },
            }
        }
        Some("starting") | Some("recovering") => OwnerStatus::Busy,
        _ => OwnerStatus::Unexpected,
    }
}

fn discover_owner(
    files: &ProjectFiles,
    project: &Path,
    settings: &Settings,
    cancel: &AtomicBool,
) -> std::result::Result<(u16, Mode), String> {
    let request = status_request(project);
    let phase_one_deadline = Instant::now() + PHASE_ONE_GRACE;
    let mut saw_connection_refused = false;
    loop {
        if cancel.load(Ordering::Acquire) {
            return Err("DAP startup cancelled".to_owned());
        }
        let remaining = phase_one_deadline
            .checked_duration_since(Instant::now())
            .unwrap_or(Duration::ZERO);
        if remaining.is_zero() {
            return Err(if saw_connection_refused {
                stale_owner_message(project)
            } else {
                no_owner_message(project)
            });
        }
        match socket_request(&files.sock, &request, remaining.min(SOCKET_REQUEST_TIMEOUT)) {
            Ok(reply) => match owner_status(&reply) {
                OwnerStatus::Ready { port, mode } => return Ok((port, mode)),
                OwnerStatus::Busy => break,
                OwnerStatus::Mismatch => return Err(mismatch_message(project)),
                OwnerStatus::Unexpected => return Err(unexpected_reply_message(project)),
            },
            Err(error) => {
                if let Some(message) = owner_error_message(&error, project) {
                    return Err(message);
                }
                if error.kind() == io::ErrorKind::ConnectionRefused {
                    saw_connection_refused = true;
                }
                thread::sleep(remaining.min(POLL_INTERVAL));
            }
        }
    }

    let deadline = (settings.startup_timeout_s != 0)
        .then(|| Instant::now() + Duration::from_secs(u64::from(settings.startup_timeout_s)));
    let mut consecutive_errors: Option<Instant> = None;
    loop {
        if cancel.load(Ordering::Acquire) {
            return Err("DAP startup cancelled".to_owned());
        }
        if deadline.is_some_and(|limit| Instant::now() >= limit) {
            return Err(format!(
                "Godot did not become ready within {}s",
                settings.startup_timeout_s
            ));
        }
        let timeout = deadline.map_or(SOCKET_REQUEST_TIMEOUT, |limit| {
            limit
                .checked_duration_since(Instant::now())
                .unwrap_or(Duration::ZERO)
                .min(SOCKET_REQUEST_TIMEOUT)
        });
        let sleep_for = deadline.map_or(POLL_INTERVAL, |limit| {
            limit
                .checked_duration_since(Instant::now())
                .unwrap_or(Duration::ZERO)
                .min(POLL_INTERVAL)
        });
        match socket_request(&files.sock, &request, timeout) {
            Ok(reply) => match owner_status(&reply) {
                OwnerStatus::Ready { port, mode } => return Ok((port, mode)),
                OwnerStatus::Busy => {
                    consecutive_errors = None;
                    thread::sleep(sleep_for);
                }
                OwnerStatus::Mismatch => return Err(mismatch_message(project)),
                OwnerStatus::Unexpected => return Err(unexpected_reply_message(project)),
            },
            Err(error) => {
                if let Some(message) = owner_error_message(&error, project) {
                    return Err(message);
                }
                let since = *consecutive_errors.get_or_insert_with(Instant::now);
                if since.elapsed() >= CONSECUTIVE_ERROR_GRACE {
                    return Err(owner_exited_message(project));
                }
                thread::sleep(sleep_for);
            }
        }
    }
}

fn refresh_owner_status(
    files: &ProjectFiles,
    project: &Path,
) -> std::result::Result<(u16, Mode), String> {
    let reply = socket_request(
        &files.sock,
        &status_request(project),
        SOCKET_REQUEST_TIMEOUT,
    )
    .map_err(|error| {
        owner_error_message(&error, project).unwrap_or_else(|| owner_exited_message(project))
    })?;
    match owner_status(&reply) {
        OwnerStatus::Ready { port, mode } => Ok((port, mode)),
        OwnerStatus::Busy => Err(format!(
            "Godot language server for {} is restarting; retry in a moment",
            project.display()
        )),
        OwnerStatus::Mismatch => Err(mismatch_message(project)),
        OwnerStatus::Unexpected => Err(unexpected_reply_message(project)),
    }
}

fn run_session_inner(
    initialize: &Value,
    connection: &mut Connection,
    output: &mut ClientOutput<std::io::Stdout>,
    input: &mut DapInput,
    buffer: &mut ClientBuffer,
    context: &DapContext,
) -> Result<ExitCode> {
    let mut server_requests = ServerRequests::new();
    if crate::json::to_vec(initialize).len() > GODOT_WRITE_CAP {
        output.failure(
            initialize.get("seq").cloned().unwrap_or(Value::Null),
            "initialize",
            "message too large for Godot",
        )?;
        return Ok(ExitCode::from(1));
    }
    send_to_godot(&mut connection.writer, initialize)?;
    match wait_for_initialize(initialize, output, input, buffer, &mut server_requests)? {
        InitializeWait::Ready => {}
        InitializeWait::ClientEof => return Ok(ExitCode::SUCCESS),
        InitializeWait::ClientInvalid => return Ok(ExitCode::from(1)),
        InitializeWait::GodotDead => {
            output.failure(
                initialize.get("seq").cloned().unwrap_or(Value::Null),
                "initialize",
                "Godot closed the debug connection before answering initialize",
            )?;
            return Ok(ExitCode::from(1));
        }
    }

    while let Some(body) = buffer.pop() {
        forward_client_body(&body, &mut server_requests, connection, context, output)?;
    }

    let mut process_seen = false;
    loop {
        match input.recv_frame()? {
            DapFrame::Body(DapSide::Godot, body) => {
                let message = parse_message(&body)?;
                if should_forward_server_event(&message, &mut process_seen) {
                    let seq = output.next_seq;
                    output.send(forward_godot_message(message, &mut server_requests, seq))?;
                }
            }
            DapFrame::Body(DapSide::Client, body) => {
                forward_client_body(&body, &mut server_requests, connection, context, output)?;
            }
            DapFrame::End(DapSide::Godot) => return godot_died(output),
            DapFrame::End(DapSide::Client) => return Ok(ExitCode::SUCCESS),
            DapFrame::Prepared(_) => unreachable!(),
        }
    }
}

fn should_forward_server_event(message: &Value, process_seen: &mut bool) -> bool {
    if message.get("type").and_then(Value::as_str) != Some("event") {
        return true;
    }
    let event = message.get("event").and_then(Value::as_str);
    if event == Some("process") {
        *process_seen = true;
        return true;
    }
    if !*process_seen && (event == Some("exited") || event == Some("terminated")) {
        crate::debug!("dropping stale DAP lifecycle event before process: {event:?}");
        return false;
    }
    true
}

fn wait_for_initialize(
    initialize: &Value,
    output: &mut ClientOutput<std::io::Stdout>,
    input: &mut DapInput,
    buffer: &mut ClientBuffer,
    server_requests: &mut ServerRequests,
) -> Result<InitializeWait> {
    let initialize_seq = initialize.get("seq").cloned().unwrap_or(Value::Null);
    loop {
        match input.recv_frame()? {
            DapFrame::Body(DapSide::Client, body) => {
                if buffer.push(&body).is_err() {
                    return Ok(InitializeWait::ClientInvalid);
                }
            }
            DapFrame::Body(DapSide::Godot, body) => {
                let message = parse_message(&body)?;
                let is_initialize_response = message.get("type").and_then(Value::as_str)
                    == Some("response")
                    && message.get("command") == Some(&crate::json!("initialize"))
                    && message.get("request_seq") == Some(&initialize_seq);
                let seq = output.next_seq;
                output.send(forward_godot_message(message, server_requests, seq))?;
                if is_initialize_response {
                    return Ok(InitializeWait::Ready);
                }
            }
            DapFrame::End(DapSide::Client) => return Ok(InitializeWait::ClientEof),
            DapFrame::End(DapSide::Godot) => return Ok(InitializeWait::GodotDead),
            DapFrame::Prepared(_) => unreachable!(),
        }
    }
}

fn forward_client_body(
    body: &[u8],
    server_requests: &mut ServerRequests,
    connection: &mut Connection,
    context: &DapContext,
    output: &mut ClientOutput<std::io::Stdout>,
) -> Result<()> {
    let fields = crate::json::scan_top_level(body)?;
    if fields.type_.is_none_or(|value| !value.is_string()) {
        return Err(crate::error::Error::new(
            "DAP message type must be a string",
        ));
    }
    if let Some(oversized) = oversized_client_write(body, &fields) {
        match oversized {
            OversizedWrite::Failure(failure) => {
                output.failure(failure.request_seq, &failure.command, &failure.message)?;
            }
            OversizedWrite::Drop => {
                crate::warn!(
                    "dropping oversized DAP message to Godot, {} bytes",
                    body.len()
                );
            }
        }
        return Ok(());
    }
    let rewrite = fields
        .type_
        .is_some_and(|value| value.string_eq("response"))
        || fields
            .command
            .is_some_and(|command| command.string_eq("launch") || command.string_eq("attach"));
    if !rewrite {
        write_frame(&mut connection.writer, body, GODOT_WRITE_CAP)?;
        return Ok(());
    }
    let failure = forward_client(parse_message(body)?, server_requests, connection, context)?;
    if let Some(failure) = failure {
        output.failure(failure.request_seq, &failure.command, &failure.message)?;
    }
    Ok(())
}

enum OversizedWrite {
    Failure(RequestFailure),
    Drop,
}

fn oversized_client_write(
    body: &[u8],
    fields: &crate::json::TopLevel<'_>,
) -> Option<OversizedWrite> {
    if body.len() <= GODOT_WRITE_CAP {
        return None;
    }
    if !fields.type_.is_some_and(|value| value.string_eq("request")) {
        return Some(OversizedWrite::Drop);
    }
    let Ok(message) = parse_message(body) else {
        return Some(OversizedWrite::Drop);
    };
    Some(OversizedWrite::Failure(RequestFailure {
        request_seq: message.get("seq").cloned().unwrap_or(Value::Null),
        command: message
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        message: "message too large for Godot".to_owned(),
    }))
}

fn forward_client(
    mut message: Value,
    server_requests: &mut ServerRequests,
    connection: &mut Connection,
    context: &DapContext,
) -> Result<Option<RequestFailure>> {
    if message.get("type").and_then(Value::as_str) == Some("response") {
        server_requests.restore_response(&mut message);
    }
    if message.get("type").and_then(Value::as_str) == Some("request") {
        let command = message
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        if command == "launch" || command == "attach" {
            match rewrite_launch_or_attach(&mut message, &command, context) {
                Ok(()) => {}
                Err((request_seq, message)) => {
                    return Ok(Some(RequestFailure {
                        request_seq,
                        command,
                        message,
                    }));
                }
            }
        }
    }
    let body = crate::json::to_vec(&message);
    if body.len() > GODOT_WRITE_CAP {
        if message.get("type").and_then(Value::as_str) != Some("request") {
            crate::warn!(
                "dropping oversized DAP message to Godot, {} bytes",
                body.len()
            );
            return Ok(None);
        }
        let request_seq = message.get("seq").cloned().unwrap_or(Value::Null);
        let command = message
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        return Ok(Some(RequestFailure {
            request_seq,
            command,
            message: "message too large for Godot".to_owned(),
        }));
    }
    write_frame(&mut connection.writer, &body, GODOT_WRITE_CAP)?;
    Ok(None)
}

fn rewrite_launch_or_attach(
    message: &mut Value,
    command: &str,
    context: &DapContext,
) -> std::result::Result<(), (Value, String)> {
    let request_seq = message.get("seq").cloned().unwrap_or(Value::Null);
    let Some(object) = message.as_object_mut() else {
        return Err((request_seq, "DAP message must be an object".to_owned()));
    };
    if object.get("arguments").is_none() {
        object.insert("arguments".to_owned(), crate::json!({}));
    }
    let Some(arguments) = object.get_mut("arguments").and_then(Value::as_object_mut) else {
        return Err((request_seq, "DAP arguments must be an object".to_owned()));
    };
    arguments.remove("adapter");
    arguments.remove("request");
    arguments.remove("file");
    arguments.remove("project");
    if command == "attach" {
        if context.mode == Mode::Headless {
            return Err((request_seq, ATTACH_PRECONDITION.to_owned()));
        }
        return Ok(());
    }

    let scene = arguments
        .get("scene")
        .and_then(Value::as_str)
        .map(str::to_owned);
    arguments.insert(
        "project".to_owned(),
        Value::String(context.project.to_string_lossy().into_owned()),
    );
    if scene.as_deref() == Some("current") {
        let Some(file) = context.file.as_deref() else {
            return Err((request_seq, "scene current requires --file".to_owned()));
        };
        let scene = resolve_scene(&context.project, file, &context.exclude)
            .map_err(|error| (request_seq.clone(), error.to_string()))?;
        arguments.insert("scene".to_owned(), Value::String(scene));
    } else if scene.is_none() || scene.as_deref() == Some("main") {
        arguments.insert("scene".to_owned(), Value::String("main".to_owned()));
    }
    Ok(())
}

fn send_to_godot(writer: &mut TcpStream, message: &Value) -> Result<()> {
    write_json(writer, message, GODOT_WRITE_CAP, true)?;
    Ok(())
}

fn forward_godot_message(
    mut message: Value,
    server_requests: &mut ServerRequests,
    bridge_seq: i64,
) -> Value {
    server_requests.restore_response(&mut message);
    server_requests.rewrite(&mut message, bridge_seq);
    message
}

fn godot_died(output: &mut ClientOutput<std::io::Stdout>) -> Result<ExitCode> {
    output.send(crate::json!({"type": "event", "event": "terminated"}))?;
    output.send(crate::json!({"type": "event", "event": "exited"}))?;
    Ok(ExitCode::from(1))
}

fn parse_message(body: &[u8]) -> std::result::Result<Value, String> {
    parse_json_object(body, "DAP")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rewrite_launch(
        message: &mut Value,
        file: Option<&Path>,
    ) -> std::result::Result<(), (Value, String)> {
        rewrite_launch_or_attach(
            message,
            "launch",
            &DapContext {
                project: PathBuf::from("/project"),
                file: file.map(Path::to_path_buf),
                mode: Mode::Headless,
                exclude: Exclude::default(),
            },
        )
    }

    #[test]
    fn attach_against_a_headless_owner_is_refused() {
        let mut message = crate::json!({
            "type": "request",
            "seq": 7,
            "command": "attach",
            "arguments": {"adapter": "godot", "request": "attach"}
        });
        let result = rewrite_launch_or_attach(
            &mut message,
            "attach",
            &DapContext {
                project: PathBuf::from("/project"),
                file: None,
                mode: Mode::Headless,
                exclude: Exclude::default(),
            },
        );
        assert_eq!(
            result,
            Err((crate::json!(7), ATTACH_PRECONDITION.to_owned()))
        );
    }

    #[test]
    fn attach_with_a_gui_or_unmanaged_owner_is_forwarded() {
        for mode in [Mode::Gui, Mode::Unmanaged] {
            let mut message = crate::json!({
                "type": "request",
                "seq": 8,
                "command": "attach",
                "arguments": {"adapter": "godot", "request": "attach", "processId": 42}
            });
            let result = rewrite_launch_or_attach(
                &mut message,
                "attach",
                &DapContext {
                    project: PathBuf::from("/project"),
                    file: None,
                    mode,
                    exclude: Exclude::default(),
                },
            );
            assert!(result.is_ok());
            assert_eq!(message["arguments"]["processId"], 42);
            assert!(message["arguments"].get("adapter").is_none());
        }
    }

    #[test]
    fn oversized_client_request_is_refused() {
        let message = crate::json!({
            "type": "request",
            "seq": 9,
            "command": "evaluate",
            "arguments": {"expression": ("x".repeat(GODOT_WRITE_CAP))}
        });
        let body = crate::json::to_vec(&message);
        let fields = crate::json::scan_top_level(&body).unwrap();
        let Some(OversizedWrite::Failure(failure)) = oversized_client_write(&body, &fields) else {
            panic!("an oversized request must be refused");
        };
        assert_eq!(failure.request_seq, crate::json!(9));
        assert_eq!(failure.command, "evaluate");
        assert_eq!(failure.message, "message too large for Godot");
    }

    #[test]
    fn oversized_client_response_is_dropped() {
        let message = crate::json!({
            "type": "response",
            "request_seq": 3,
            "body": {"value": ("x".repeat(GODOT_WRITE_CAP))}
        });
        let body = crate::json::to_vec(&message);
        let fields = crate::json::scan_top_level(&body).unwrap();
        assert!(matches!(
            oversized_client_write(&body, &fields),
            Some(OversizedWrite::Drop)
        ));
    }

    #[test]
    fn owner_errors_are_classified() {
        let project = Path::new("/project");
        assert!(owner_error_message(&io::Error::from(io::ErrorKind::TimedOut), project).is_none());
        assert!(owner_error_message(&io::Error::from(io::ErrorKind::NotFound), project).is_none());
        assert!(
            owner_error_message(&io::Error::from(io::ErrorKind::ConnectionRefused), project)
                .is_none()
        );
        let denied =
            owner_error_message(&io::Error::from(io::ErrorKind::PermissionDenied), project)
                .expect("permission denied fails fast");
        assert!(
            denied.contains("cannot talk to the Godot language server"),
            "{denied}"
        );
        let decode = owner_error_message(&io::Error::other("bad json"), project)
            .expect("a decode error fails fast");
        assert!(decode.contains("unexpected owner reply"), "{decode}");
    }

    #[test]
    fn owner_status_replies_are_classified() {
        assert!(matches!(
            owner_status(&crate::json!({"status":"ready","dap_port":4000,"mode":"headless"})),
            OwnerStatus::Ready {
                port: 4000,
                mode: Mode::Headless
            }
        ));
        assert!(matches!(
            owner_status(&crate::json!({"status":"ready","dap_port":4000,"mode":"gui"})),
            OwnerStatus::Ready {
                port: 4000,
                mode: Mode::Gui
            }
        ));
        assert!(matches!(
            owner_status(&crate::json!({"status":"ready","dap_port":4000})),
            OwnerStatus::Ready {
                port: 4000,
                mode: Mode::Unmanaged
            }
        ));
        assert!(matches!(
            owner_status(&crate::json!({"status":"starting"})),
            OwnerStatus::Busy
        ));
        assert!(matches!(
            owner_status(&crate::json!({"status":"recovering"})),
            OwnerStatus::Busy
        ));
        assert!(matches!(
            owner_status(&crate::json!({"error":"project mismatch"})),
            OwnerStatus::Mismatch
        ));
        assert!(matches!(
            owner_status(&crate::json!({"status":"ready"})),
            OwnerStatus::Unexpected
        ));
        assert!(matches!(
            owner_status(&crate::json!({"unexpected": true})),
            OwnerStatus::Unexpected
        ));
    }

    #[test]
    fn rewrites_launch_arguments() {
        let mut message = crate::json!({
            "type": "request",
            "seq": 4,
            "command": "launch",
            "arguments": {
                "adapter": "godot",
                "request": "launch",
                "file": "main.gd",
                "project": "wrong",
                "scene": "main"
            }
        });
        assert!(rewrite_launch(&mut message, None).is_ok());
        assert_eq!(message["arguments"]["project"], "/project");
        assert_eq!(message["arguments"]["scene"], "main");
        assert!(message["arguments"].get("adapter").is_none());
    }

    #[test]
    fn missing_current_file_has_exact_error() {
        let mut message = crate::json!({
            "type": "request",
            "seq": 2,
            "command": "launch",
            "arguments": {"scene": "current"}
        });
        assert_eq!(
            rewrite_launch(&mut message, None),
            Err((crate::json!(2), "scene current requires --file".to_owned()))
        );
    }

    #[test]
    fn server_request_sequence_is_restored() {
        let mut requests = ServerRequests::new();
        let mut request = crate::json!({"type":"request","seq":17});
        requests.rewrite(&mut request, 1);
        let mut response = crate::json!({"type":"response","request_seq":1});
        requests.restore_response(&mut response);
        assert_eq!(response["request_seq"], 17);
    }

    #[test]
    fn drops_lifecycle_events_before_process() {
        let messages = [
            crate::json!({"type":"event","event":"exited"}),
            crate::json!({"type":"event","event":"terminated"}),
            crate::json!({"type":"event","event":"process"}),
            crate::json!({"type":"event","event":"exited"}),
            crate::json!({"type":"event","event":"terminated"}),
        ];
        let mut process_seen = false;
        let forwarded = messages
            .iter()
            .filter(|message| should_forward_server_event(message, &mut process_seen))
            .map(|message| message["event"].clone())
            .collect::<Vec<_>>();
        assert_eq!(
            forwarded,
            [
                crate::json!("process"),
                crate::json!("exited"),
                crate::json!("terminated"),
            ]
        );
    }
}
