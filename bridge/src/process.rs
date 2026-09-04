//! Godot process spawning, readiness, output, and termination.

use std::collections::VecDeque;
use std::io;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::process::{Child, Command};
use tokio::sync::Mutex as AsyncMutex;

const LOG_LIMIT: u64 = 20 * 1024 * 1024;
const TAIL_LIMIT: usize = 20;
const GROUP_WAIT: Duration = Duration::from_secs(5);

/// A Godot editor process owned by the bridge.
pub struct GodotChild {
    /// The editor process ID.
    pub pid: u32,
    /// The editor process group ID.
    pub pgid: i32,
    /// The editor process start time from `/proc/<pid>/stat`.
    pub start_ticks: u64,
    /// The child handle used to reap the editor.
    pub child: Child,
    /// The most recent output lines from the editor.
    pub tail: Arc<Mutex<VecDeque<String>>>,
}

impl GodotChild {
    /// Returns the most recent 20 lines written by the editor.
    pub fn last_lines(&self) -> Vec<String> {
        self.tail
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .cloned()
            .collect()
    }
}

/// Selects an available loopback TCP port from `range`.
pub fn pick_free_port(range: std::ops::RangeInclusive<u16>) -> io::Result<u16> {
    let mut last_error = None;
    for port in range {
        match TcpListener::bind(("127.0.0.1", port)) {
            Ok(listener) => {
                drop(listener);
                return Ok(port);
            }
            Err(error) => last_error = Some(error),
        }
    }

    Err(last_error
        .unwrap_or_else(|| io::Error::new(io::ErrorKind::AddrNotAvailable, "port range is empty")))
}

/// Spawns a headless Godot editor and captures its output.
pub fn spawn_godot(
    bin: impl AsRef<Path>,
    extra_args: &[String],
    project: impl AsRef<Path>,
    lsp_port: u16,
    dap_port: u16,
    log_path: impl AsRef<Path>,
) -> io::Result<GodotChild> {
    let parent_pid = unsafe { libc::getpid() };
    let project = project.as_ref();
    let mut command = Command::new(bin.as_ref());
    command
        .args(extra_args)
        .arg("--editor")
        .arg("--headless")
        .arg("--path")
        .arg(project)
        .arg("--lsp-port")
        .arg(lsp_port.to_string())
        .arg("--dap-port")
        .arg(dap_port.to_string())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    unsafe {
        command.pre_exec(move || {
            if libc::setpgid(0, 0) == -1 {
                return Err(io::Error::last_os_error());
            }
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) == -1 {
                return Err(io::Error::last_os_error());
            }
            if libc::getppid() != parent_pid {
                libc::_exit(1);
            }
            Ok(())
        });
    }

    let mut child = command.spawn()?;
    let pid = child
        .id()
        .ok_or_else(|| io::Error::other("spawned Godot process has no pid"))?;
    let start_ticks = match process_start_ticks(pid) {
        Ok(ticks) => ticks,
        Err(error) => {
            let _ = child.start_kill();
            return Err(error);
        }
    };
    let tail = Arc::new(Mutex::new(VecDeque::with_capacity(TAIL_LIMIT)));
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    spawn_output_task(
        stdout,
        stderr,
        log_path.as_ref().to_owned(),
        Arc::clone(&tail),
    );

    Ok(GodotChild {
        pid,
        pgid: pid as i32,
        start_ticks,
        child,
        tail,
    })
}

/// Spawns a detached GUI Godot editor and returns its process identity.
pub fn spawn_gui(
    bin: impl AsRef<Path>,
    extra_args: &[String],
    project: impl AsRef<Path>,
    lsp_port: u16,
    dap_port: u16,
    log_path: impl AsRef<Path>,
) -> io::Result<(u32, i32, u64)> {
    let output = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path.as_ref())?;
    let error_output = output.try_clone()?;
    let project = project.as_ref();
    let mut command = Command::new(bin.as_ref());
    command
        .args(extra_args)
        .arg("--editor")
        .arg("--path")
        .arg(project)
        .arg("--lsp-port")
        .arg(lsp_port.to_string())
        .arg("--dap-port")
        .arg(dap_port.to_string())
        .stdout(Stdio::from(output))
        .stderr(Stdio::from(error_output));

    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let child = command.spawn()?;
    let pid = child
        .id()
        .ok_or_else(|| io::Error::other("spawned GUI process has no pid"))?;
    let start_ticks = process_start_ticks(pid)?;
    Ok((pid, pid as i32, start_ticks))
}

/// Reports the result of waiting for a Godot TCP port.
pub enum Readiness {
    /// The port accepted a connection.
    Ready(TcpStream),
    /// The child exited before the port became ready.
    ChildExited(ExitStatus),
    /// The optional deadline elapsed first.
    Deadline,
}

/// Polls a Godot port until it accepts a connection, exits, or reaches `deadline`.
pub async fn wait_for_port(
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

        if let Ok(stream) = TcpStream::connect(("127.0.0.1", port)).await {
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
        tokio::time::sleep(sleep_for).await;
    }
}

/// Terminates and reaps an owned Godot process group.
pub async fn kill_group(mut child: GodotChild) -> io::Result<()> {
    signal_group(child.pgid, libc::SIGTERM)?;
    match tokio::time::timeout(GROUP_WAIT, child.child.wait()).await {
        Ok(status) => {
            status?;
        }
        Err(_) => {
            signal_group(child.pgid, libc::SIGKILL)?;
            child.child.wait().await?;
        }
    }
    Ok(())
}

/// Terminates a detached process group after verifying its recorded identity.
pub async fn kill_recorded(pid: u32, pgid: i32, ticks: u64) -> io::Result<()> {
    if process_start_ticks(pid).ok() != Some(ticks) {
        return Ok(());
    }

    signal_group(pgid, libc::SIGTERM)?;
    if wait_for_process_to_disappear(pid, ticks, GROUP_WAIT).await {
        return Ok(());
    }

    signal_group(pgid, libc::SIGKILL)?;
    while process_start_ticks(pid).ok() == Some(ticks) {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Ok(())
}

fn process_start_ticks(pid: u32) -> io::Result<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))?;
    let end = stat
        .rfind(')')
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid process stat"))?;
    stat[end + 1..]
        .split_whitespace()
        .nth(19)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing process start time"))?
        .parse()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid process start time"))
}

fn signal_group(pgid: i32, signal: i32) -> io::Result<()> {
    if pgid <= 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "process group id must be positive",
        ));
    }
    if unsafe { libc::kill(-pgid, signal) } == -1 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            return Err(error);
        }
    }
    Ok(())
}

async fn wait_for_process_to_disappear(pid: u32, ticks: u64, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if process_start_ticks(pid).ok() != Some(ticks) {
            return true;
        }
        let remaining = match deadline.checked_duration_since(Instant::now()) {
            Some(remaining) => remaining,
            None => return false,
        };
        tokio::time::sleep(remaining.min(Duration::from_millis(50))).await;
    }
}

fn spawn_output_task(
    stdout: Option<tokio::process::ChildStdout>,
    stderr: Option<tokio::process::ChildStderr>,
    log_path: PathBuf,
    tail: Arc<Mutex<VecDeque<String>>>,
) {
    tokio::spawn(async move {
        let writer = match LogWriter::open(log_path).await {
            Ok(writer) => Arc::new(AsyncMutex::new(writer)),
            Err(error) => {
                tracing::warn!(%error, "cannot open Godot log");
                return;
            }
        };

        let stdout_task = stdout.map(|stream| {
            tokio::spawn(copy_output(stream, Arc::clone(&writer), Arc::clone(&tail)))
        });
        let stderr_task = stderr.map(|stream| {
            tokio::spawn(copy_output(stream, Arc::clone(&writer), Arc::clone(&tail)))
        });

        match (stdout_task, stderr_task) {
            (Some(stdout_task), Some(stderr_task)) => {
                let _ = tokio::join!(stdout_task, stderr_task);
            }
            (Some(stdout_task), None) => {
                let _ = stdout_task.await;
            }
            (None, Some(stderr_task)) => {
                let _ = stderr_task.await;
            }
            (None, None) => {}
        }
    });
}

async fn copy_output<R>(
    stream: R,
    writer: Arc<AsyncMutex<LogWriter>>,
    tail: Arc<Mutex<VecDeque<String>>>,
) -> io::Result<()>
where
    R: AsyncRead + Unpin,
{
    let mut reader = BufReader::new(stream);
    let mut line = Vec::new();
    loop {
        line.clear();
        let bytes_read = reader.read_until(b'\n', &mut line).await?;
        if bytes_read == 0 {
            return Ok(());
        }

        let mut text = String::from_utf8_lossy(&line).into_owned();
        while text.ends_with('\n') || text.ends_with('\r') {
            text.pop();
        }
        {
            let mut lines = tail.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            lines.push_back(text);
            while lines.len() > TAIL_LIMIT {
                lines.pop_front();
            }
        }
        writer.lock().await.write(&line).await?;
    }
}

struct LogWriter {
    file: Option<tokio::fs::File>,
    path: PathBuf,
    length: u64,
}

impl LogWriter {
    async fn open(path: PathBuf) -> io::Result<Self> {
        let length = match tokio::fs::metadata(&path).await {
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
            writer.rotate().await?;
        }
        writer.file = Some(
            tokio::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&writer.path)
                .await?,
        );
        Ok(writer)
    }

    async fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        let mut offset = 0;
        while offset < bytes.len() {
            if self.length >= LOG_LIMIT {
                self.rotate().await?;
                self.file = Some(
                    tokio::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&self.path)
                        .await?,
                );
            }
            let available = (LOG_LIMIT - self.length) as usize;
            let size = available.min(bytes.len() - offset);
            self.file
                .as_mut()
                .expect("log file is open")
                .write_all(&bytes[offset..offset + size])
                .await?;
            self.length += size as u64;
            offset += size;
        }
        Ok(())
    }

    async fn rotate(&mut self) -> io::Result<()> {
        if let Some(mut file) = self.file.take() {
            file.flush().await?;
        }
        let rotated = PathBuf::from(format!("{}.1", self.path.display()));
        match tokio::fs::rename(&self.path, rotated).await {
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
    use std::path::Path;

    use tempfile::tempdir;

    #[test]
    fn picked_port_can_be_bound_again() {
        let port = pick_free_port(41000..=41000).expect("port should be free");
        TcpListener::bind(("127.0.0.1", port)).expect("picked port should be bindable");
    }

    #[tokio::test]
    async fn readiness_reports_child_exit() {
        let directory = tempdir().expect("temporary directory");
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
        .await
        .expect("readiness poll");
        assert!(matches!(readiness, Readiness::ChildExited(_)));
    }

    #[tokio::test]
    async fn group_kill_terminates_grandchild() {
        let directory = tempdir().expect("temporary directory");
        let script = "/bin/sleep 30 & printf '%s\\n' \"$!\" >&2; wait";
        let args = vec!["-c".to_owned(), script.to_owned()];
        let child = spawn_godot(
            Path::new("/bin/sh"),
            &args,
            directory.path(),
            41004,
            41005,
            directory.path().join("godot.log"),
        )
        .expect("shell should spawn");

        let grandchild_pid = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                if let Some(line) = child.last_lines().last() {
                    if let Ok(pid) = line.parse::<u32>() {
                        break pid;
                    }
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "grandchild pid should be logged: tail={:?}",
                child.last_lines()
            )
        });

        kill_group(child).await.expect("group should be killed");
        let gone = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                if !Path::new(&format!("/proc/{grandchild_pid}")).exists() {
                    break true;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("grandchild should exit");
        assert!(gone);
    }
}
