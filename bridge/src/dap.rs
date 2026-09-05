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

use crate::framing::{
    connection_without_reader, parse_json_object, spawn_frame_reader_with, write_frame, write_json,
    Connection, FrameDecoder, FrameError, ReadEvent,
};
use crate::root::{cwd_root, find_project_dir};
use crate::scene::resolve_scene;
use crate::settings_file::{parse_trusted_settings, Settings};
use crate::state::{socket_request, try_lock, LockGuard, ProjectFiles};

const FRAME_CAP: usize = 64 * 1024 * 1024;
const BUFFER_CAP: usize = 64 * 1024 * 1024;
const SOCKET_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const POLL_INTERVAL: Duration = Duration::from_millis(500);

struct Prepared {
    connection: Connection,
    lock: LockGuard,
    project: PathBuf,
    file: Option<PathBuf>,
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

    fn failure(&mut self, initialize: &Value, message: &str) -> Result<()> {
        self.send(crate::json!({
            "type": "response",
            "request_seq": (initialize.get("seq").cloned().unwrap_or(Value::Null)),
            "command": "initialize",
            "success": false,
            "message": message,
        }))
    }
}

struct ServerRequests {
    original_sequences: HashMap<crate::json::RequestKey, Value>,
    order: VecDeque<crate::json::RequestKey>,
}

impl ServerRequests {
    fn new() -> Self {
        Self {
            original_sequences: HashMap::new(),
            order: VecDeque::new(),
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
                    self.order.push_back(key);
                    while self.order.len() > 4096 {
                        if let Some(old) = self.order.pop_front() {
                            self.original_sequences.remove(&old);
                        }
                    }
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
    let _input_thread = spawn_frame_reader_with(
        "godot-bridge-dap-client-reader",
        std::io::stdin(),
        sender.clone(),
        DapReadEvent::Client,
    )?;
    let mut input = DapInput::new(receiver);
    let initialize = loop {
        match input.recv_frame()? {
            DapFrame::Body(DapSide::Client, body) => {
                let message = match parse_message(&body) {
                    Ok(message) => message,
                    Err(_) => return Ok(ExitCode::from(1)),
                };
                if message.get("type").and_then(Value::as_str) == Some("request")
                    && message.get("command").and_then(Value::as_str) == Some("initialize")
                {
                    break message;
                }
                return Ok(ExitCode::from(1));
            }
            DapFrame::Body(side, body) => input.defer(DapFrame::Body(side, body)),
            DapFrame::End(DapSide::Client) => return Ok(ExitCode::SUCCESS),
            DapFrame::End(DapSide::Godot) => return Ok(ExitCode::from(1)),
            DapFrame::Prepared(_) => return Ok(ExitCode::from(1)),
        }
    };

    let mut output = ClientOutput::new(std::io::stdout());
    let mut buffer = ClientBuffer::new();
    let mut early_frames = VecDeque::new();
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
        match input.recv_frame()? {
            DapFrame::Body(DapSide::Client, body) => {
                if buffer.push(&body).is_err() {
                    cancel.store(true, Ordering::Release);
                    return Ok(ExitCode::from(1));
                }
            }
            DapFrame::Body(side, body) => early_frames.push_back(DapFrame::Body(side, body)),
            DapFrame::End(side) => {
                early_frames.push_back(DapFrame::End(side));
                if matches!(side, DapSide::Client) {
                    cancel.store(true, Ordering::Release);
                    return Ok(ExitCode::from(1));
                }
            }
            DapFrame::Prepared(result) => break result,
        }
    };
    let mut prepared = match prepared {
        Ok(prepared) => prepared,
        Err(message) => {
            output.failure(&initialize, &message)?;
            return Ok(ExitCode::from(1));
        }
    };
    for frame in early_frames {
        input.defer(frame);
    }
    let project = prepared.project.clone();
    let file = prepared.file.clone();
    let result = run_session_inner(
        &initialize,
        &mut prepared.connection,
        &mut output,
        &mut input,
        &mut buffer,
        &project,
        file.as_deref(),
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
    let settings = read_settings(&root)?;
    let project = find_project_dir(&root, file, settings.project_dir.as_deref().map(Path::new))
        .map_err(|error| error.to_string())?;
    if let Some(file) = file {
        let normalized = crate::root::canonical_or_normalized(file);
        if !normalized.starts_with(&root) {
            return Err(format!(
                "DAP file {} is outside the worktree",
                file.display()
            ));
        }
    }
    let files = ProjectFiles::new(&project).map_err(|error| error.to_string())?;
    let lock = match try_lock(&files.dap_lock).map_err(|error| error.to_string())? {
        Some(guard) => guard,
        None => {
            return Err(format!(
                "A debug session for {} is already running",
                project.display()
            ));
        }
    };

    let stream = if settings.lsp_port.is_some() {
        connect_dap(settings.dap_port)?
    } else {
        discover_owner(&files, &project, &settings, cancel)?
    };
    let connection = connection_without_reader(
        stream,
        "godot-bridge-dap-reader",
        sender,
        DapReadEvent::Godot,
    )
    .map_err(|error| error.to_string())?;
    Ok(Prepared {
        connection,
        lock,
        project,
        file: file.map(crate::root::canonical_or_normalized),
    })
}

fn read_settings(worktree: &Path) -> std::result::Result<Settings, String> {
    let value = match std::env::var("GODOT_BRIDGE_SETTINGS") {
        Ok(contents) if contents.len() <= 1024 * 1024 => crate::json::from_str(&contents)
            .map_err(|error| format!("invalid GODOT_BRIDGE_SETTINGS: {error}"))?,
        Ok(_) => return Err("GODOT_BRIDGE_SETTINGS exceeds 1 MiB".to_owned()),
        Err(std::env::VarError::NotPresent) => Value::Null,
        Err(error) => return Err(format!("cannot read GODOT_BRIDGE_SETTINGS: {error}")),
    };
    parse_trusted_settings(&value, worktree)
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

fn discover_owner(
    files: &ProjectFiles,
    project: &Path,
    settings: &Settings,
    cancel: &AtomicBool,
) -> std::result::Result<TcpStream, String> {
    let no_owner = format!(
        "No Godot language server runs for {}. Open a .gd file of the project in Zed first.",
        project.display()
    );
    let deadline = (settings.startup_timeout_s != 0)
        .then(|| Instant::now() + Duration::from_secs(u64::from(settings.startup_timeout_s)));
    loop {
        if cancel.load(Ordering::Acquire) {
            return Err("DAP startup cancelled".to_owned());
        }
        let timeout = deadline.map_or(SOCKET_REQUEST_TIMEOUT, |limit| {
            limit
                .checked_duration_since(Instant::now())
                .unwrap_or(Duration::ZERO)
                .min(SOCKET_REQUEST_TIMEOUT)
        });
        if timeout.is_zero() {
            return Err(format!(
                "Godot did not become ready within {}s",
                settings.startup_timeout_s
            ));
        }
        let status = socket_request(
            &files.sock,
            &crate::json!({"cmd": "status", "project": (project.to_string_lossy().into_owned())}),
            timeout,
        )
        .map_err(|_| no_owner.clone())?;
        match status.get("status").and_then(Value::as_str) {
            Some("ready") => {
                let port = status
                    .get("dap_port")
                    .and_then(Value::as_u64)
                    .and_then(|port| u16::try_from(port).ok())
                    .ok_or_else(|| "Godot owner has no DAP port".to_owned())?;
                return connect_dap(port);
            }
            Some("starting") | Some("recovering") => {
                if deadline.is_some_and(|limit| Instant::now() >= limit) {
                    return Err(format!(
                        "Godot did not become ready within {}s",
                        settings.startup_timeout_s
                    ));
                }
                let sleep_for = deadline.map_or(POLL_INTERVAL, |limit| {
                    limit
                        .checked_duration_since(Instant::now())
                        .unwrap_or(Duration::ZERO)
                        .min(POLL_INTERVAL)
                });
                thread::sleep(sleep_for);
            }
            _ => return Err(no_owner),
        }
    }
}

fn run_session_inner(
    initialize: &Value,
    connection: &mut Connection,
    output: &mut ClientOutput<std::io::Stdout>,
    input: &mut DapInput,
    buffer: &mut ClientBuffer,
    project: &Path,
    file: Option<&Path>,
) -> Result<ExitCode> {
    let mut server_requests = ServerRequests::new();
    send_to_godot(&mut connection.writer, initialize)?;
    match wait_for_initialize(initialize, output, input, buffer, &mut server_requests)? {
        InitializeWait::Ready => {}
        InitializeWait::ClientEof => return Ok(ExitCode::SUCCESS),
        InitializeWait::ClientInvalid | InitializeWait::GodotDead => {
            return Ok(ExitCode::from(1));
        }
    }

    while let Some(body) = buffer.pop() {
        if let Some(result) =
            forward_client_body(&body, &mut server_requests, connection, project, file)?
        {
            send_request_failure(output, result)?;
        }
    }

    let mut process_seen = false;
    loop {
        match input.recv_frame()? {
            DapFrame::Body(DapSide::Godot, body) => {
                let mut message = parse_message(&body)?;
                if should_forward_server_event(&message, &mut process_seen) {
                    server_requests.restore_response(&mut message);
                    let seq = output.next_seq;
                    server_requests.rewrite(&mut message, seq);
                    output.send(message)?;
                }
            }
            DapFrame::Body(DapSide::Client, body) => {
                if let Some(failure) =
                    forward_client_body(&body, &mut server_requests, connection, project, file)?
                {
                    send_request_failure(output, failure)?;
                }
            }
            DapFrame::End(DapSide::Godot) => return godot_died(output),
            DapFrame::End(DapSide::Client) => return Ok(ExitCode::SUCCESS),
            DapFrame::Prepared(_) => return Ok(ExitCode::from(1)),
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
                let mut message = parse_message(&body)?;
                let is_initialize_response = message.get("type").and_then(Value::as_str)
                    == Some("response")
                    && message.get("command") == Some(&crate::json!("initialize"))
                    && message.get("request_seq") == Some(&initialize_seq);
                server_requests.restore_response(&mut message);
                let seq = output.next_seq;
                server_requests.rewrite(&mut message, seq);
                output.send(message)?;
                if is_initialize_response {
                    return Ok(InitializeWait::Ready);
                }
            }
            DapFrame::End(DapSide::Client) => return Ok(InitializeWait::ClientEof),
            DapFrame::End(DapSide::Godot) => return Ok(InitializeWait::GodotDead),
            DapFrame::Prepared(_) => return Ok(InitializeWait::GodotDead),
        }
    }
}

fn forward_client_body(
    body: &[u8],
    server_requests: &mut ServerRequests,
    connection: &mut Connection,
    project: &Path,
    file: Option<&Path>,
) -> Result<Option<RequestFailure>> {
    let fields = crate::json::scan_top_level(body)?;
    if fields.type_.is_none_or(|value| !value.is_string()) {
        return Err(crate::error::Error::new(
            "DAP message type must be a string",
        ));
    }
    let rewrite = fields
        .type_
        .is_some_and(|value| value.string_eq("response"))
        || fields
            .command
            .is_some_and(|command| command.string_eq("launch") || command.string_eq("attach"));
    if rewrite {
        return forward_client(
            parse_message(body)?,
            server_requests,
            connection,
            project,
            file,
        );
    }
    send_to_godot_body(&mut connection.writer, body)
}

fn forward_client(
    mut message: Value,
    server_requests: &mut ServerRequests,
    connection: &mut Connection,
    project: &Path,
    file: Option<&Path>,
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
            match rewrite_launch_or_attach(&mut message, &command, project, file) {
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
    send_to_godot(&mut connection.writer, &message)?;
    Ok(None)
}

fn send_request_failure(
    output: &mut ClientOutput<std::io::Stdout>,
    failure: RequestFailure,
) -> Result<()> {
    output.send(crate::json!({
        "type": "response",
        "request_seq": (failure.request_seq),
        "command": (failure.command),
        "success": false,
        "message": (failure.message),
    }))
}

fn rewrite_launch_or_attach(
    message: &mut Value,
    command: &str,
    project: &Path,
    file: Option<&Path>,
) -> std::result::Result<(), (Value, String)> {
    let request_seq = message.get("seq").cloned().unwrap_or(Value::Null);
    let Some(object) = message.as_object_mut() else {
        return Err((request_seq, "DAP message must be an object".to_owned()));
    };
    if command != "launch" {
        if let Some(arguments) = object.get_mut("arguments").and_then(Value::as_object_mut) {
            arguments.remove("adapter");
            arguments.remove("request");
            arguments.remove("file");
            arguments.remove("project");
        }
        return Ok(());
    }
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

    let scene = arguments
        .get("scene")
        .and_then(Value::as_str)
        .map(str::to_owned);
    arguments.insert(
        "project".to_owned(),
        Value::String(project.to_string_lossy().into_owned()),
    );
    if scene.as_deref() == Some("current") {
        let Some(file) = file else {
            return Err((request_seq, "scene current requires --file".to_owned()));
        };
        let scene = resolve_scene(project, file)
            .map_err(|error| (request_seq.clone(), error.to_string()))?;
        arguments.insert("scene".to_owned(), Value::String(scene));
    } else if scene.is_none() || scene.as_deref() == Some("main") {
        arguments.insert("scene".to_owned(), Value::String("main".to_owned()));
    }
    Ok(())
}

fn send_to_godot(writer: &mut TcpStream, message: &Value) -> Result<()> {
    write_json(writer, message, FRAME_CAP, true)?;
    Ok(())
}

fn send_to_godot_body(writer: &mut TcpStream, body: &[u8]) -> Result<Option<RequestFailure>> {
    write_frame(writer, body, FRAME_CAP)?;
    Ok(None)
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
        assert!(
            rewrite_launch_or_attach(&mut message, "launch", Path::new("/project"), None).is_ok()
        );
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
            rewrite_launch_or_attach(&mut message, "launch", Path::new("/project"), None),
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
