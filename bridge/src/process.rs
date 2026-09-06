use std::collections::VecDeque;
use std::io;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::sys::{self, OpenMode};

pub use crate::sys::port_listener_belongs_to_process;

const LOG_LIMIT: u64 = 20 * 1024 * 1024;
const TAIL_LIMIT: usize = 20;

fn open_log(path: &Path) -> io::Result<std::fs::File> {
    sys::open_private(path, OpenMode::Append)
}

pub struct GodotChild {
    pub pid: u32,
    pub pgid: u32,
    pub start_ticks: u64,
    pub child: Child,
    pub tail: Arc<Mutex<VecDeque<String>>>,
    output_threads: Vec<JoinHandle<()>>,
}

impl GodotChild {
    pub fn last_lines(&self) -> Vec<String> {
        self.tail
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .cloned()
            .collect()
    }

    pub fn wait_output(&mut self) {
        for thread in self.output_threads.drain(..) {
            let _ = thread.join();
        }
    }
}

pub fn pick_free_port(range: std::ops::RangeInclusive<u16>) -> io::Result<u16> {
    let start = *range.start();
    let end = *range.end();
    if start > end {
        return Err(io::Error::new(
            io::ErrorKind::AddrNotAvailable,
            "port range is empty",
        ));
    }
    let count = usize::from(end - start) + 1;
    let offset = std::process::id() as usize % count;
    let mut last_error = None;
    for index in 0..count {
        let port = start + ((offset + index) % count) as u16;
        match TcpListener::bind(("127.0.0.1", port)) {
            Ok(listener) => {
                drop(listener);
                return Ok(port);
            }
            Err(error) => last_error = Some(error),
        }
    }

    Err(last_error.expect("port range has at least one port"))
}

fn editor_command(
    bin: &Path,
    extra_args: &[String],
    project: &Path,
    lsp_port: u16,
    dap_port: u16,
    headless: bool,
) -> Command {
    let mut command = Command::new(bin);
    command.args(extra_args).arg("--editor");
    if headless {
        command.arg("--headless");
    }
    command
        .arg("--path")
        .arg(project)
        .arg("--lsp-port")
        .arg(lsp_port.to_string())
        .arg("--dap-port")
        .arg(dap_port.to_string());
    command
}

pub fn spawn_godot(
    bin: impl AsRef<Path>,
    extra_args: &[String],
    project: impl AsRef<Path>,
    lsp_port: u16,
    dap_port: u16,
    log_path: impl AsRef<Path>,
) -> io::Result<GodotChild> {
    let mut command = editor_command(
        bin.as_ref(),
        extra_args,
        project.as_ref(),
        lsp_port,
        dap_port,
        true,
    );
    let (mut child, pgid, start_ticks) = sys::spawn_headless_process(&mut command)?;
    let tail = Arc::new(Mutex::new(VecDeque::with_capacity(TAIL_LIMIT)));
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let output_threads = spawn_output_task(
        stdout,
        stderr,
        log_path.as_ref().to_owned(),
        Arc::clone(&tail),
    );

    Ok(GodotChild {
        pid: child.id(),
        pgid,
        start_ticks,
        child,
        tail,
        output_threads,
    })
}

pub fn spawn_gui(
    bin: impl AsRef<Path>,
    extra_args: &[String],
    project: impl AsRef<Path>,
    lsp_port: u16,
    dap_port: u16,
    log_path: impl AsRef<Path>,
) -> io::Result<(u32, u32, u64)> {
    let output = open_log(log_path.as_ref())?;
    let mut command = editor_command(
        bin.as_ref(),
        extra_args,
        project.as_ref(),
        lsp_port,
        dap_port,
        false,
    );
    let (child, pgid, start_ticks) = sys::spawn_gui_process(&mut command, output)?;
    Ok((child.id(), pgid, start_ticks))
}

pub enum Readiness {
    Ready(TcpStream),
    ChildExited(ExitStatus),
    Deadline,
}

pub fn wait_for_port(
    child: &mut GodotChild,
    port: u16,
    deadline: Option<Instant>,
) -> io::Result<Readiness> {
    loop {
        if let Some(status) = child.child.try_wait()? {
            return Ok(Readiness::ChildExited(status));
        }
        if deadline.is_some_and(|limit| Instant::now() >= limit) {
            return Ok(Readiness::Deadline);
        }

        if let Ok(stream) = TcpStream::connect_timeout(
            &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
            Duration::from_millis(50),
        ) {
            return Ok(Readiness::Ready(stream));
        }

        let poll = Duration::from_millis(200);
        let sleep_for = deadline.map_or(poll, |limit| {
            limit
                .checked_duration_since(Instant::now())
                .map_or(Duration::ZERO, |remaining| remaining.min(poll))
        });
        if sleep_for.is_zero() {
            return Ok(Readiness::Deadline);
        }
        thread::sleep(sleep_for);
    }
}

pub fn kill_group(mut child: GodotChild) -> io::Result<()> {
    let result = sys::kill_group(&mut child.child, child.pid, child.pgid, child.start_ticks);
    let _ = child.child.wait();
    child.wait_output();
    result
}

pub fn kill_recorded(pid: u32, pgid: u32, ticks: u64) -> io::Result<()> {
    sys::kill_recorded(pid, pgid, ticks)
}

fn spawn_output_task(
    stdout: Option<std::process::ChildStdout>,
    stderr: Option<std::process::ChildStderr>,
    log_path: PathBuf,
    tail: Arc<Mutex<VecDeque<String>>>,
) -> Vec<JoinHandle<()>> {
    let writer = Arc::new(Mutex::new(match LogWriter::open(log_path) {
        Ok(writer) => writer,
        Err(error) => {
            crate::warn!("cannot open Godot log: {error}");
            LogWriter::disabled()
        }
    }));
    let mut readers = Vec::new();
    if let Some(stream) = stdout {
        readers.push(spawn_output_reader(
            stream,
            Arc::clone(&writer),
            Arc::clone(&tail),
        ));
    }
    if let Some(stream) = stderr {
        readers.push(spawn_output_reader(stream, writer, tail));
    }
    readers
}

fn spawn_output_reader<R: Read + Send + 'static>(
    stream: R,
    writer: Arc<Mutex<LogWriter>>,
    tail: Arc<Mutex<VecDeque<String>>>,
) -> JoinHandle<()> {
    thread::Builder::new()
        .name("godot-bridge-output-reader".to_owned())
        .stack_size(256 * 1024)
        .spawn(move || {
            let mut reader = stream;
            let mut bytes = [0u8; 8192];
            let mut line = Vec::new();
            loop {
                match reader.read(&mut bytes) {
                    Ok(0) | Err(_) => break,
                    Ok(count) => {
                        if let Err(error) = writer
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .write(&bytes[..count])
                        {
                            crate::warn!("cannot write Godot log: {error}");
                            writer
                                .lock()
                                .unwrap_or_else(|poisoned| poisoned.into_inner())
                                .disable();
                        }
                        for &byte in &bytes[..count] {
                            if byte == b'\n' {
                                append_tail(&tail, &line);
                                line.clear();
                            } else if line.len() < TAIL_LINE_LIMIT {
                                line.push(byte);
                            }
                        }
                    }
                }
            }
            if !line.is_empty() {
                append_tail(&tail, &line);
            }
        })
        .expect("output reader thread should spawn")
}

const TAIL_LINE_LIMIT: usize = 16 * 1024;

fn append_tail(tail: &Arc<Mutex<VecDeque<String>>>, bytes: &[u8]) {
    let mut text = String::from_utf8_lossy(bytes).into_owned();
    while text.ends_with('\r') {
        text.pop();
    }
    let mut lines = tail.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    lines.push_back(text);
    while lines.len() > TAIL_LIMIT {
        lines.pop_front();
    }
}

struct LogWriter {
    file: Option<std::fs::File>,
    path: PathBuf,
    length: u64,
}

impl LogWriter {
    fn disabled() -> Self {
        Self {
            file: None,
            path: PathBuf::new(),
            length: 0,
        }
    }

    fn disable(&mut self) {
        self.file = None;
    }

    fn open(path: PathBuf) -> io::Result<Self> {
        let length = match std::fs::metadata(&path) {
            Ok(metadata) => metadata.len(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => 0,
            Err(error) => return Err(error),
        };
        let mut writer = Self {
            file: None,
            path,
            length,
        };
        if length >= LOG_LIMIT {
            writer.rotate()?;
        }
        writer.file = Some(open_log(&writer.path)?);
        Ok(writer)
    }

    fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        if self.file.is_none() {
            return Ok(());
        }
        let mut offset = 0;
        while offset < bytes.len() {
            if self.length >= LOG_LIMIT {
                self.rotate()?;
                self.file = Some(open_log(&self.path)?);
            }
            let available = (LOG_LIMIT - self.length) as usize;
            let size = available.min(bytes.len() - offset);
            self.file
                .as_mut()
                .expect("log file is open")
                .write_all(&bytes[offset..offset + size])?;
            self.length += size as u64;
            offset += size;
        }
        Ok(())
    }

    fn rotate(&mut self) -> io::Result<()> {
        if let Some(mut file) = self.file.take() {
            file.flush()?;
        }
        let rotated = PathBuf::from(format!("{}.1", self.path.display()));
        match std::fs::rename(&self.path, rotated) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        self.length = 0;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    use crate::temp::TempDir;

    #[test]
    fn picked_port_can_be_bound_again() {
        for _ in 0..5 {
            let port = pick_free_port(41000..=41100).expect("port should be free");
            if TcpListener::bind(("127.0.0.1", port)).is_ok() {
                return;
            }
        }
        panic!("picked port should be bindable");
    }

    #[cfg(unix)]
    #[test]
    fn listener_owner_matches_process() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(port_listener_belongs_to_process(std::process::id(), port).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn readiness_reports_child_exit() {
        let directory = TempDir::new().expect("temporary directory");
        let args = vec!["-c".to_owned(), "exit 0".to_owned()];
        let mut child = spawn_godot(
            Path::new("/bin/sh"),
            &args,
            directory.path(),
            41001,
            41002,
            directory.path().join("godot.log"),
        )
        .expect("shell should spawn");

        let readiness = wait_for_port(
            &mut child,
            41003,
            Some(Instant::now() + Duration::from_secs(2)),
        )
        .expect("readiness poll");
        assert!(matches!(readiness, Readiness::ChildExited(_)));
    }

    #[cfg(unix)]
    #[test]
    fn group_kill_terminates_grandchild() {
        assert_group_kill_reaches_grandchild(
            "/bin/sleep 300 & printf '%s\\n' \"$!\" >&2; wait",
            41004,
        );
    }

    #[cfg(unix)]
    #[test]
    fn group_kill_reaches_grandchild_after_leader_exits() {
        assert_group_kill_reaches_grandchild(
            "/bin/sh -c 'trap \"\" TERM; sleep 300' & printf '%s\\n' \"$!\" >&2; trap 'exit 0' TERM; wait",
            41006,
        );
    }

    #[cfg(unix)]
    fn assert_group_kill_reaches_grandchild(script: &str, port: u16) {
        let directory = TempDir::new().expect("temporary directory");
        let args = vec!["-c".to_owned(), script.to_owned()];
        let child = spawn_godot(
            Path::new("/bin/sh"),
            &args,
            directory.path(),
            port,
            port + 1,
            directory.path().join("godot.log"),
        )
        .expect("shell should spawn");

        let deadline = Instant::now() + Duration::from_secs(30);
        let grandchild_pid = loop {
            if let Some(line) = child.last_lines().last() {
                if let Ok(pid) = line.parse::<u32>() {
                    break pid;
                }
            }
            assert!(Instant::now() < deadline, "grandchild pid should be logged");
            thread::sleep(Duration::from_millis(10));
        };

        kill_group(child).expect("group should be killed");
        let deadline = Instant::now() + Duration::from_secs(10);
        while Path::new(&format!("/proc/{grandchild_pid}")).exists() {
            assert!(Instant::now() < deadline, "grandchild should exit");
            thread::sleep(Duration::from_millis(10));
        }
    }
}
