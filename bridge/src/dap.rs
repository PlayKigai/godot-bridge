use crate::error::Result;
use crate::json::Value;
use std::collections::{HashMap, VecDeque};
use std::io::{self, BufWriter, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, TryRecvError};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::framing::{
    parse_json_object, spawn_frame_reader, write_frame, write_json, FrameInput, FramePoll,
};
use crate::root::{cwd_root, find_project_dir};
use crate::scene::resolve_scene;
use crate::settings_file::{parse_settings, Settings};
use crate::state::{socket_request, try_lock, LockGuard, ProjectFiles};

const FRAME_CAP: usize = 64 * 1024 * 1024;
const BUFFER_CAP: usize = 64 * 1024 * 1024;
const SOCKET_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const POLL_INTERVAL: Duration = Duration::from_millis(500);

struct Prepared {
    connection: Connection,
    lock: DapLock,
    project: PathBuf,
    file: Option<PathBuf>,
}

struct Connection {
    socket: TcpStream,
    reader: Option<FrameInput>,
    reader_thread: Option<JoinHandle<()>>,
    writer: TcpStream,
}

impl Connection {
    fn from_stream(stream: TcpStream) -> io::Result<Self> {
        let reader_stream = stream.try_clone()?;
        let writer = stream.try_clone()?;
        let (sender, receiver) = mpsc::sync_channel(1);
        let reader_thread =
            spawn_frame_reader("godot-bridge-dap-reader", reader_stream, FRAME_CAP, sender)?;
        Ok(Self {
            socket: stream,
            reader: Some(FrameInput::new(receiver, FRAME_CAP)),
            reader_thread: Some(reader_thread),
            writer,
        })
    }

    fn close(&mut self) {
        let _ = self.socket.shutdown(std::net::Shutdown::Both);
        drop(self.reader.take());
        if let Some(reader_thread) = self.reader_thread.take() {
            let _ = reader_thread.join();
        }
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.close();
    }
}

struct DapLock {
    guard: Option<LockGuard>,
}

impl Drop for DapLock {
    fn drop(&mut self) {
        self.guard.take();
    }
}

struct ClientBuffer {
    messages: VecDeque<Vec<u8>>,
    bytes: usize,
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
}

impl ServerRequests {
    fn new() -> Self {
        Self {
            original_sequences: HashMap::new(),
        }
    }

    fn rewrite(&mut self, message: &mut Value, bridge_seq: i64) {
        if message.get("type").and_then(Value::as_str) == Some("request") {
            if let Some(original) = message.get("seq").cloned() {
                self.original_sequences
                    .insert(crate::json::RequestKey::Number(bridge_seq), original);
            }
        }
        message["seq"] = crate::json!(bridge_seq);
    }

    fn restore_response(&mut self, message: &mut Value) {
        if message.get("type").and_then(Value::as_str) != Some("response") {
            return;
        }
        let Some(request_seq) = message.get("request_seq") else {
            return;
        };
        if let Some(original) = self
            .original_sequences
            .remove(&crate::json::value_request_key(request_seq))
        {
            message["request_seq"] = original;
        }
    }
}

enum ForwardClient {
    Failure {
        request_seq: Value,
        command: String,
        message: String,
    },
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
        FRAME_CAP,
        sender,
    )?;
    let mut input = FrameInput::new(receiver, FRAME_CAP);
    let initialize = match input.with_next_frame(parse_message)? {
        Some(Ok(message))
            if message.get("type").and_then(Value::as_str) == Some("request")
                && message.get("command").and_then(Value::as_str) == Some("initialize") =>
        {
            message
        }
        Some(Ok(_)) | Some(Err(_)) => return Ok(ExitCode::from(1)),
        None => return Ok(ExitCode::SUCCESS),
    };

    let mut output = ClientOutput::new(std::io::stdout());
    let mut buffer = ClientBuffer::new();
    let cancel = Arc::new(AtomicBool::new(false));
    let cancel_for_worker = Arc::clone(&cancel);
    let (prepared_sender, prepared_receiver) = mpsc::channel();
    let worker = thread::Builder::new()
        .name("godot-bridge-dap-prepare".to_owned())
        .stack_size(256 * 1024)
        .spawn(move || {
            let result = prepare(file.as_deref(), &cancel_for_worker);
            let _ = prepared_sender.send(result);
        })?;
    let prepared = loop {
        match prepared_receiver.try_recv() {
            Ok(result) => break result,
            Err(TryRecvError::Disconnected) => {
                return Ok(ExitCode::from(1));
            }
            Err(TryRecvError::Empty) => {}
        }
        match input.recv_timeout_with_frame(Duration::from_millis(20), |body| buffer.push(body))? {
            FramePoll::Frame(Ok(())) => {}
            FramePoll::Frame(Err(())) => {
                cancel.store(true, Ordering::Release);
                return Ok(ExitCode::from(1));
            }
            FramePoll::End => {
                cancel.store(true, Ordering::Release);
                return Ok(ExitCode::from(1));
            }
            FramePoll::Empty => {}
        }
    };
    let prepared = match prepared {
        Ok(prepared) => prepared,
        Err(message) => {
            output.failure(&initialize, &message)?;
            return Ok(ExitCode::from(1));
        }
    };
    let result = run_session(initialize, prepared, &mut output, &mut input, &mut buffer);
    let _ = worker.join();
    result
}

fn prepare(file: Option<&Path>, cancel: &AtomicBool) -> std::result::Result<Prepared, String> {
    if cancel.load(Ordering::Acquire) {
        return Err("DAP startup cancelled".to_owned());
    }
    let settings = read_settings()?;
    let root = cwd_root().map_err(|error| error.to_string())?;
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
        Some(guard) => DapLock { guard: Some(guard) },
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
    let connection = Connection::from_stream(stream).map_err(|error| error.to_string())?;
    Ok(Prepared {
        connection,
        lock,
        project,
        file: file.map(crate::root::canonical_or_normalized),
    })
}

fn read_settings() -> std::result::Result<Settings, String> {
    let value = match std::env::var("GODOT_BRIDGE_SETTINGS") {
        Ok(contents) if contents.len() <= 1024 * 1024 => crate::json::from_str(&contents)
            .map_err(|error| format!("invalid GODOT_BRIDGE_SETTINGS: {error}"))?,
        Ok(_) => return Err("GODOT_BRIDGE_SETTINGS exceeds 1 MiB".to_owned()),
        Err(std::env::VarError::NotPresent) => Value::Null,
        Err(error) => return Err(format!("cannot read GODOT_BRIDGE_SETTINGS: {error}")),
    };
    parse_settings(&value)
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
            &crate::json!({"cmd": "status", "project": (project.to_string_lossy())}),
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

fn run_session(
    initialize: Value,
    mut prepared: Prepared,
    output: &mut ClientOutput<std::io::Stdout>,
    input: &mut FrameInput,
    buffer: &mut ClientBuffer,
) -> Result<ExitCode> {
    let project = prepared.project.clone();
    let file = prepared.file.clone();
    let result = run_session_inner(
        &initialize,
        &mut prepared.connection,
        output,
        input,
        buffer,
        &project,
        file.as_deref(),
    );
    drop(prepared.lock);
    result
}

fn run_session_inner(
    initialize: &Value,
    connection: &mut Connection,
    output: &mut ClientOutput<std::io::Stdout>,
    input: &mut FrameInput,
    buffer: &mut ClientBuffer,
    project: &Path,
    file: Option<&Path>,
) -> Result<ExitCode> {
    let mut server_requests = ServerRequests::new();
    send_to_godot(&mut connection.writer, initialize)?;
    match wait_for_initialize(
        initialize,
        connection,
        output,
        input,
        buffer,
        &mut server_requests,
    )? {
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
            let ForwardClient::Failure {
                request_seq,
                command,
                message,
            } = result;
            send_request_failure(output, request_seq, command, message)?;
        }
    }

    let mut process_seen = false;
    let mut turn = false;
    loop {
        let mut processed = false;
        if turn {
            let event = connection
                .reader
                .as_mut()
                .ok_or_else(|| crate::error::Error::new("reader is closed"))?
                .try_with_next_frame(parse_message)?;
            match event {
                FramePoll::Frame(Ok(mut message)) => {
                    processed = true;
                    if should_forward_server_event(&message, &mut process_seen) {
                        server_requests.restore_response(&mut message);
                        let seq = output.next_seq;
                        server_requests.rewrite(&mut message, seq);
                        output.send(message)?;
                    }
                }
                FramePoll::Frame(Err(_)) | FramePoll::End => return godot_died(output),
                FramePoll::Empty => {}
            }
        } else {
            let event = input.try_with_next_frame(|body| {
                forward_client_body(body, &mut server_requests, connection, project, file)
            })?;
            match event {
                FramePoll::Frame(Ok(result)) => {
                    processed = true;
                    if let Some(ForwardClient::Failure {
                        request_seq,
                        command,
                        message,
                    }) = result
                    {
                        send_request_failure(output, request_seq, command, message)?;
                    }
                }
                FramePoll::Frame(Err(_)) => return Ok(ExitCode::from(1)),
                FramePoll::End => return Ok(ExitCode::SUCCESS),
                FramePoll::Empty => {}
            }
        }
        turn = !turn;
        if processed {
            continue;
        }
        if !turn {
            match input.recv_timeout_with_frame(Duration::from_millis(20), |body| {
                forward_client_body(body, &mut server_requests, connection, project, file)
            })? {
                FramePoll::Frame(Ok(Some(ForwardClient::Failure {
                    request_seq,
                    command,
                    message,
                }))) => send_request_failure(output, request_seq, command, message)?,
                FramePoll::Frame(Ok(None)) => {}
                FramePoll::Frame(Err(_)) => return Ok(ExitCode::from(1)),
                FramePoll::End => return Ok(ExitCode::SUCCESS),
                FramePoll::Empty => {}
            }
        } else {
            thread::sleep(Duration::from_millis(20));
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
    connection: &mut Connection,
    output: &mut ClientOutput<std::io::Stdout>,
    input: &mut FrameInput,
    buffer: &mut ClientBuffer,
    server_requests: &mut ServerRequests,
) -> Result<InitializeWait> {
    let initialize_seq = initialize.get("seq").cloned().unwrap_or(Value::Null);
    let mut turn = false;
    loop {
        let mut processed = false;
        if turn {
            let event = connection
                .reader
                .as_mut()
                .ok_or_else(|| crate::error::Error::new("reader is closed"))?
                .try_with_next_frame(parse_message)?;
            match event {
                FramePoll::Frame(Ok(mut message)) => {
                    processed = true;
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
                FramePoll::Frame(Err(_)) | FramePoll::End => return Ok(InitializeWait::GodotDead),
                FramePoll::Empty => {}
            }
        } else {
            match input.try_with_next_frame(|body| buffer.push(body))? {
                FramePoll::Frame(Ok(())) => processed = true,
                FramePoll::Frame(Err(())) => return Ok(InitializeWait::ClientInvalid),
                FramePoll::End => return Ok(InitializeWait::ClientEof),
                FramePoll::Empty => {}
            }
        }
        turn = !turn;
        if processed {
            continue;
        }
        if turn {
            match input
                .recv_timeout_with_frame(Duration::from_millis(20), |body| buffer.push(body))?
            {
                FramePoll::Frame(Ok(())) => {}
                FramePoll::Frame(Err(())) => return Ok(InitializeWait::ClientInvalid),
                FramePoll::End => return Ok(InitializeWait::ClientEof),
                FramePoll::Empty => {}
            }
        } else {
            thread::sleep(Duration::from_millis(20));
        }
    }
}

fn forward_client_body(
    body: &[u8],
    server_requests: &mut ServerRequests,
    connection: &mut Connection,
    project: &Path,
    file: Option<&Path>,
) -> Result<Option<ForwardClient>> {
    let fields = crate::json::scan_top_level(body)?;
    let rewrite = fields.type_.is_some_and(|value| {
        value.string_eq("response")
            || (value.string_eq("request")
                && fields.command.is_some_and(|command| {
                    command.string_eq("launch") || command.string_eq("attach")
                }))
    });
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
) -> Result<Option<ForwardClient>> {
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
                    return Ok(Some(ForwardClient::Failure {
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
    request_seq: Value,
    command: String,
    message: String,
) -> Result<()> {
    output.send(crate::json!({
        "type": "response",
        "request_seq": request_seq,
        "command": command,
        "success": false,
        "message": message,
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

fn send_to_godot_body(writer: &mut TcpStream, body: &[u8]) -> Result<Option<ForwardClient>> {
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
