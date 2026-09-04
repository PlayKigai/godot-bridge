use crate::error::{Error, Result};
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};
use tokio::io::{AsyncWrite, BufWriter};
use tokio::net::{tcp::OwnedWriteHalf, TcpStream};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::framing::{parse_json_object, write_json, FrameReader};
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
    reader: FrameReader<tokio::net::tcp::OwnedReadHalf>,
    writer: OwnedWriteHalf,
}

struct DapLock {
    guard: Option<LockGuard>,
}

impl Drop for DapLock {
    fn drop(&mut self) {
        self.guard.take();
    }
}

enum InputEvent {
    Frame(Vec<u8>),
    Eof,
    Error,
}

struct ClientBuffer {
    messages: VecDeque<Value>,
    bytes: usize,
}

impl ClientBuffer {
    fn new() -> Self {
        Self {
            messages: VecDeque::new(),
            bytes: 0,
        }
    }

    fn push(&mut self, body: Vec<u8>) -> std::result::Result<(), ()> {
        let size = body.len();
        if self
            .bytes
            .checked_add(size)
            .is_none_or(|total| total > BUFFER_CAP)
        {
            return Err(());
        }
        let message = parse_message(&body).map_err(|_| ())?;
        self.bytes += size;
        self.messages.push_back(message);
        Ok(())
    }

    fn pop(&mut self) -> Option<Value> {
        let message = self.messages.pop_front()?;
        self.bytes = self
            .bytes
            .saturating_sub(serde_json::to_vec(&message).map_or(0, |bytes| bytes.len()));
        Some(message)
    }
}

struct ClientOutput<W> {
    writer: BufWriter<W>,
    next_seq: i64,
}

impl<W: AsyncWrite + Unpin> ClientOutput<W> {
    fn new(writer: W) -> Self {
        Self {
            writer: BufWriter::new(writer),
            next_seq: 1,
        }
    }

    async fn send(&mut self, mut message: Value) -> Result<()> {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.saturating_add(1);
        message["seq"] = json!(seq);
        write_json(&mut self.writer, &message, FRAME_CAP, true)
            .await
            .map_err(Error::new)?;
        Ok(())
    }

    async fn failure(&mut self, initialize: &Value, message: &str) -> Result<()> {
        self.send(json!({
            "type": "response",
            "request_seq": initialize.get("seq").cloned().unwrap_or(Value::Null),
            "command": "initialize",
            "success": false,
            "message": message,
        }))
        .await
    }
}

struct ServerRequests {
    original_sequences: HashMap<i64, Value>,
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
                self.original_sequences.insert(bridge_seq, original);
            }
        }
        message["seq"] = json!(bridge_seq);
    }

    fn restore_response(&mut self, message: &mut Value) {
        if message.get("type").and_then(Value::as_str) != Some("response") {
            return;
        }
        let Some(request_seq) = message.get("request_seq").and_then(Value::as_i64) else {
            return;
        };
        if let Some(original) = self.original_sequences.remove(&request_seq) {
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

pub async fn run(file: Option<PathBuf>, trailing: Vec<String>) -> crate::error::Result<ExitCode> {
    let _ = trailing;
    let mut input = FrameReader::new(tokio::io::stdin(), FRAME_CAP);
    let first = match input.read_frame().await {
        Ok(Some(body)) => body,
        Ok(None) => return Ok(ExitCode::SUCCESS),
        Err(_) => return Ok(ExitCode::from(1)),
    };
    let initialize = match parse_message(&first) {
        Ok(message)
            if message.get("type").and_then(Value::as_str) == Some("request")
                && message.get("command").and_then(Value::as_str) == Some("initialize") =>
        {
            message
        }
        Ok(_) | Err(_) => return Ok(ExitCode::from(1)),
    };

    let (sender, mut receiver) = mpsc::channel(1);
    let reader_task = tokio::spawn(read_client_frames(input, sender));
    let mut output = ClientOutput::new(tokio::io::stdout());
    let mut buffer = ClientBuffer::new();
    let mut preparing = Box::pin(prepare(file.as_deref()));
    let prepared = loop {
        tokio::select! {
            result = &mut preparing => break result,
            event = receiver.recv() => {
                match event {
                    Some(InputEvent::Frame(body)) => {
                        if buffer.push(body).is_err() {
                            stop_reader(reader_task).await;
                            return Ok(ExitCode::from(1));
                        }
                    }
                    Some(InputEvent::Eof) | None => {
                        stop_reader(reader_task).await;
                        return Ok(ExitCode::SUCCESS);
                    }
                    Some(InputEvent::Error) => {
                        stop_reader(reader_task).await;
                        return Ok(ExitCode::from(1));
                    }
                }
            }
        }
    };
    let prepared = match prepared {
        Ok(prepared) => prepared,
        Err(message) => {
            stop_reader(reader_task).await;
            output.failure(&initialize, &message).await?;
            return Ok(ExitCode::from(1));
        }
    };

    let result = run_session(
        initialize,
        prepared,
        &mut output,
        &mut receiver,
        &mut buffer,
    )
    .await;
    stop_reader(reader_task).await;
    result
}

async fn prepare(file: Option<&Path>) -> std::result::Result<Prepared, String> {
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
        connect_dap(settings.dap_port).await?
    } else {
        discover_owner(&files, &project, &settings).await?
    };
    let (read, write) = stream.into_split();
    Ok(Prepared {
        connection: Connection {
            reader: FrameReader::new(read, FRAME_CAP),
            writer: write,
        },
        lock,
        project,
        file: file.map(crate::root::canonical_or_normalized),
    })
}

fn read_settings() -> std::result::Result<Settings, String> {
    let value = match std::env::var("GODOT_BRIDGE_SETTINGS") {
        Ok(contents) if contents.len() <= 1024 * 1024 => serde_json::from_str(&contents)
            .map_err(|error| format!("invalid GODOT_BRIDGE_SETTINGS: {error}"))?,
        Ok(_) => return Err("GODOT_BRIDGE_SETTINGS exceeds 1 MiB".to_owned()),
        Err(std::env::VarError::NotPresent) => Value::Null,
        Err(error) => return Err(format!("cannot read GODOT_BRIDGE_SETTINGS: {error}")),
    };
    parse_settings(&value)
}

async fn connect_dap(port: u16) -> std::result::Result<TcpStream, String> {
    match tokio::time::timeout(
        SOCKET_REQUEST_TIMEOUT,
        TcpStream::connect(("127.0.0.1", port)),
    )
    .await
    {
        Ok(Ok(stream)) => Ok(stream),
        Ok(Err(error)) => Err(format!(
            "cannot connect to Godot DAP on port {port}: {error}"
        )),
        Err(_) => Err(format!(
            "cannot connect to Godot DAP on port {port}: timed out"
        )),
    }
}

async fn discover_owner(
    files: &ProjectFiles,
    project: &Path,
    settings: &Settings,
) -> std::result::Result<TcpStream, String> {
    let no_owner = format!(
        "No Godot language server runs for {}. Open a .gd file of the project in Zed first.",
        project.display()
    );
    let deadline = (settings.startup_timeout_s != 0)
        .then(|| Instant::now() + Duration::from_secs(u64::from(settings.startup_timeout_s)));
    loop {
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
        let status = socket_request(&files.sock, &json!({"cmd": "status"}), timeout)
            .await
            .map_err(|_| no_owner.clone())?;
        match status.get("status").and_then(Value::as_str) {
            Some("ready") => {
                let port = status
                    .get("dap_port")
                    .and_then(Value::as_u64)
                    .and_then(|port| u16::try_from(port).ok())
                    .ok_or_else(|| "Godot owner has no DAP port".to_owned())?;
                return connect_dap(port).await;
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
                tokio::time::sleep(sleep_for).await;
            }
            _ => return Err(no_owner),
        }
    }
}

async fn run_session(
    initialize: Value,
    mut prepared: Prepared,
    output: &mut ClientOutput<tokio::io::Stdout>,
    receiver: &mut mpsc::Receiver<InputEvent>,
    buffer: &mut ClientBuffer,
) -> Result<ExitCode> {
    let project = prepared.project.clone();
    let file = prepared.file.clone();
    let result = run_session_inner(
        &initialize,
        &mut prepared.connection,
        output,
        receiver,
        buffer,
        &project,
        file.as_deref(),
    )
    .await;
    drop(prepared.lock);
    result
}

async fn run_session_inner(
    initialize: &Value,
    connection: &mut Connection,
    output: &mut ClientOutput<tokio::io::Stdout>,
    receiver: &mut mpsc::Receiver<InputEvent>,
    buffer: &mut ClientBuffer,
    project: &Path,
    file: Option<&Path>,
) -> Result<ExitCode> {
    let mut server_requests = ServerRequests::new();
    send_to_godot(&mut connection.writer, initialize).await?;
    match wait_for_initialize(
        initialize,
        connection,
        output,
        receiver,
        buffer,
        &mut server_requests,
    )
    .await?
    {
        InitializeWait::Ready => {}
        InitializeWait::ClientEof => return Ok(ExitCode::SUCCESS),
        InitializeWait::ClientInvalid | InitializeWait::GodotDead => {
            return Ok(ExitCode::from(1));
        }
    }

    while let Some(message) = buffer.pop() {
        if let Some(result) =
            forward_client(message, &mut server_requests, connection, project, file).await?
        {
            let ForwardClient::Failure {
                request_seq,
                command,
                message,
            } = result;
            send_request_failure(output, request_seq, command, message).await?;
        }
    }

    let mut process_seen = false;
    loop {
        tokio::select! {
            event = receiver.recv() => {
                match event {
                    Some(InputEvent::Frame(body)) => {
                        let message = match parse_message(&body) {
                            Ok(message) => message,
                            Err(_) => return Ok(ExitCode::from(1)),
                        };
                        if let Some(result) = forward_client(
                            message,
                            &mut server_requests,
                            connection,
                            project,
                            file,
                        ).await? {
                            let ForwardClient::Failure { request_seq, command, message } = result;
                            send_request_failure(output, request_seq, command, message).await?;
                        }
                    }
                    Some(InputEvent::Eof) | None => return Ok(ExitCode::SUCCESS),
                    Some(InputEvent::Error) => return Ok(ExitCode::from(1)),
                }
            }
            server = connection.reader.read_frame() => {
                match server {
                    Ok(Some(body)) => {
                        let mut message = match parse_message(&body) {
                            Ok(message) => message,
                            Err(_) => return godot_died(output).await,
                        };
                        if !should_forward_server_event(&message, &mut process_seen) {
                            continue;
                        }
                        server_requests.restore_response(&mut message);
                        let seq = output.next_seq;
                        server_requests.rewrite(&mut message, seq);
                        output.send(message).await?;
                    }
                    Ok(None) | Err(_) => return godot_died(output).await,
                }
            }
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

async fn wait_for_initialize(
    initialize: &Value,
    connection: &mut Connection,
    output: &mut ClientOutput<tokio::io::Stdout>,
    receiver: &mut mpsc::Receiver<InputEvent>,
    buffer: &mut ClientBuffer,
    server_requests: &mut ServerRequests,
) -> Result<InitializeWait> {
    let initialize_seq = initialize.get("seq").cloned().unwrap_or(Value::Null);
    loop {
        tokio::select! {
            event = receiver.recv() => {
                match event {
                    Some(InputEvent::Frame(body)) => {
                        if buffer.push(body).is_err() {
                            return Ok(InitializeWait::ClientInvalid);
                        }
                    }
                    Some(InputEvent::Eof) | None => return Ok(InitializeWait::ClientEof),
                    Some(InputEvent::Error) => return Ok(InitializeWait::ClientInvalid),
                }
            }
            server = connection.reader.read_frame() => {
                match server {
                    Ok(Some(body)) => {
                        let mut message = match parse_message(&body) {
                            Ok(message) => message,
                            Err(_) => {
                                godot_died(output).await?;
                                return Ok(InitializeWait::GodotDead);
                            }
                        };
                        let is_initialize_response = message.get("type").and_then(Value::as_str) == Some("response")
                            && message.get("command") == Some(&json!("initialize"))
                            && message.get("request_seq") == Some(&initialize_seq);
                        server_requests.restore_response(&mut message);
                        let seq = output.next_seq;
                        server_requests.rewrite(&mut message, seq);
                        output.send(message).await?;
                        if is_initialize_response {
                            return Ok(InitializeWait::Ready);
                        }
                    }
                    Ok(None) | Err(_) => {
                        godot_died(output).await?;
                        return Ok(InitializeWait::GodotDead);
                    }
                }
            }
        }
    }
}

async fn forward_client(
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
    send_to_godot(&mut connection.writer, &message).await?;
    Ok(None)
}

async fn send_request_failure(
    output: &mut ClientOutput<tokio::io::Stdout>,
    request_seq: Value,
    command: String,
    message: String,
) -> Result<()> {
    output
        .send(json!({
            "type": "response",
            "request_seq": request_seq,
            "command": command,
            "success": false,
            "message": message,
        }))
        .await
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
    let Some(arguments) = object
        .entry("arguments")
        .or_insert_with(|| json!({}))
        .as_object_mut()
    else {
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

async fn send_to_godot(writer: &mut OwnedWriteHalf, message: &Value) -> Result<()> {
    write_json(writer, message, FRAME_CAP, true)
        .await
        .map_err(Error::new)?;
    Ok(())
}

async fn godot_died(output: &mut ClientOutput<tokio::io::Stdout>) -> Result<ExitCode> {
    output
        .send(json!({"type": "event", "event": "terminated"}))
        .await?;
    output
        .send(json!({"type": "event", "event": "exited"}))
        .await?;
    Ok(ExitCode::from(1))
}

async fn read_client_frames(
    mut input: FrameReader<tokio::io::Stdin>,
    sender: mpsc::Sender<InputEvent>,
) {
    loop {
        match input.read_frame().await {
            Ok(Some(body)) => {
                if sender.send(InputEvent::Frame(body)).await.is_err() {
                    return;
                }
            }
            Ok(None) => {
                let _ = sender.send(InputEvent::Eof).await;
                return;
            }
            Err(_error) => {
                let _ = sender.send(InputEvent::Error).await;
                return;
            }
        }
    }
}

async fn stop_reader(reader: JoinHandle<()>) {
    reader.abort();
    let _ = reader.await;
}

fn parse_message(body: &[u8]) -> std::result::Result<Value, String> {
    parse_json_object(body, "DAP")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrites_launch_arguments() {
        let mut message = json!({
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
        let mut message = json!({
            "type": "request",
            "seq": 2,
            "command": "launch",
            "arguments": {"scene": "current"}
        });
        assert_eq!(
            rewrite_launch_or_attach(&mut message, "launch", Path::new("/project"), None),
            Err((json!(2), "scene current requires --file".to_owned()))
        );
    }

    #[test]
    fn server_request_sequence_is_restored() {
        let mut requests = ServerRequests::new();
        let mut request = json!({"type":"request","seq":17});
        requests.rewrite(&mut request, 1);
        let mut response = json!({"type":"response","request_seq":1});
        requests.restore_response(&mut response);
        assert_eq!(response["request_seq"], 17);
    }

    #[test]
    fn drops_lifecycle_events_before_process() {
        let messages = [
            json!({"type":"event","event":"exited"}),
            json!({"type":"event","event":"terminated"}),
            json!({"type":"event","event":"process"}),
            json!({"type":"event","event":"exited"}),
            json!({"type":"event","event":"terminated"}),
        ];
        let mut process_seen = false;
        let forwarded = messages
            .iter()
            .filter(|message| should_forward_server_event(message, &mut process_seen))
            .map(|message| message["event"].clone())
            .collect::<Vec<_>>();
        assert_eq!(
            forwarded,
            [json!("process"), json!("exited"), json!("terminated")]
        );
    }
}
