//! Process identity, spawning into an own process group, and group teardown.

use std::collections::HashSet;
use std::io;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const GROUP_WAIT: Duration = Duration::from_secs(5);

/// A value that changes when a pid is reused, so a recorded pid can be checked
/// before it is signalled. Linux uses field 22 of `/proc/<pid>/stat`.
pub fn process_start_ticks(pid: u32) -> io::Result<u64> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/stat"))?;
    let after_name = text
        .rsplit_once(')')
        .map(|(_, rest)| rest)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid process stat"))?;
    after_name
        .split_whitespace()
        .nth(19)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing process start time"))?
        .parse()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid process start time"))
}

/// Whether the process recorded as `pid` with `ticks` is still that same
/// process, so a reused pid is never mistaken for the original.
pub fn pid_alive_with_ticks(pid: u32, ticks: u64) -> bool {
    process_start_ticks(pid).ok() == Some(ticks)
}

/// Whether `pid` owns the listening socket on `port`, so the bridge never
/// talks to a stranger that grabbed the port first.
pub fn port_listener_belongs_to_process(pid: u32, port: u16) -> io::Result<bool> {
    let mut inodes = HashSet::new();
    for path in ["/proc/net/tcp", "/proc/net/tcp6"] {
        let contents = std::fs::read_to_string(path)?;
        for line in contents.lines().skip(1) {
            let fields = line.split_whitespace().collect::<Vec<_>>();
            if fields.len() <= 9 || fields[3] != "0A" {
                continue;
            }
            let Some((_, port_hex)) = fields[1].split_once(':') else {
                continue;
            };
            if u16::from_str_radix(port_hex, 16).ok() == Some(port) {
                if let Ok(inode) = fields[9].parse::<u64>() {
                    inodes.insert(inode);
                }
            }
        }
    }
    if inodes.is_empty() {
        return Ok(false);
    }
    for entry in std::fs::read_dir(format!("/proc/{pid}/fd"))? {
        let entry = entry?;
        let target = match std::fs::read_link(entry.path()) {
            Ok(target) => target,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        let Some(inode) = target
            .to_str()
            .and_then(|target| target.strip_prefix("socket:["))
            .and_then(|target| target.strip_suffix(']'))
            .and_then(|inode| inode.parse::<u64>().ok())
        else {
            continue;
        };
        if inodes.contains(&inode) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Spawn the headless editor as the leader of a new process group that dies
/// with the bridge. Returns the child, its process group id and its identity.
pub fn spawn_headless_process(command: &mut Command) -> io::Result<(Child, u32, u64)> {
    let parent_pid = std::process::id();
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    unsafe {
        command.pre_exec(move || {
            if libc::setpgid(0, 0) == -1 {
                libc::_exit(127);
            }
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) == -1 {
                libc::_exit(127);
            }
            if libc::getppid() as u32 != parent_pid {
                libc::_exit(127);
            }
            Ok(())
        });
    }
    spawn_with_identity(command)
}

/// Spawn the GUI editor in its own session so it outlives the bridge. Both of
/// its output streams go to `log`.
pub fn spawn_gui_process(
    command: &mut Command,
    log: std::fs::File,
) -> io::Result<(Child, u32, u64)> {
    let error_output = log.try_clone()?;
    command
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(error_output));
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                libc::_exit(127);
            }
            Ok(())
        });
    }
    spawn_with_identity(command)
}

fn spawn_with_identity(command: &mut Command) -> io::Result<(Child, u32, u64)> {
    let mut child = command.spawn()?;
    let pid = child.id();
    match process_start_ticks(pid) {
        Ok(ticks) => Ok((child, pid, ticks)),
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            Err(error)
        }
    }
}

/// Terminate the whole process group of a child the bridge still owns. The
/// caller reaps the child afterwards.
pub fn kill_group(_child: &mut Child, pid: u32, pgid: u32, ticks: u64) -> io::Result<()> {
    let group = validate_ids(pid, pgid)?;
    if !signal_group(pid, group, ticks, libc::SIGTERM)? {
        return Ok(());
    }
    let deadline = Instant::now() + GROUP_WAIT;
    while !leader_exited_unreaped(pid)? {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            break;
        };
        thread::sleep(remaining.min(Duration::from_millis(50)));
    }
    // The unreaped leader pins the pgid, so -pgid cannot name a foreign group here.
    if unsafe { libc::kill(-group, libc::SIGKILL) } != 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            return Err(error);
        }
    }
    Ok(())
}

/// Terminate a process group recorded in a state file, which this process may
/// never have spawned.
pub fn kill_recorded(pid: u32, pgid: u32, ticks: u64) -> io::Result<()> {
    let group = validate_ids(pid, pgid)?;
    if !pid_alive_with_ticks(pid, ticks) {
        return Ok(());
    }
    if !signal_group(pid, group, ticks, libc::SIGTERM)? {
        return Ok(());
    }
    if wait_for_process_to_disappear(pid, ticks, GROUP_WAIT) {
        return Ok(());
    }
    if !signal_group(pid, group, ticks, libc::SIGKILL)? {
        return Ok(());
    }
    if !wait_for_process_to_disappear(pid, ticks, GROUP_WAIT) {
        crate::warn!("process {pid} did not exit after SIGKILL");
    }
    Ok(())
}

fn leader_exited_unreaped(pid: u32) -> io::Result<bool> {
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let rc = unsafe {
        libc::waitid(
            libc::P_PID,
            pid,
            &mut info,
            libc::WEXITED | libc::WNOWAIT | libc::WNOHANG,
        )
    };
    if rc == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { info.si_pid() } != 0)
}

fn validate_ids(pid: u32, pgid: u32) -> io::Result<libc::pid_t> {
    let invalid = || {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "process group id must be greater than 1 and match the leader",
        )
    };
    if pid <= 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "process id must be greater than 1",
        ));
    }
    if pgid <= 1 || pgid != pid {
        return Err(invalid());
    }
    libc::pid_t::try_from(pgid).map_err(|_| invalid())
}

fn signal_group(pid: u32, pgid: libc::pid_t, ticks: u64, signal: libc::c_int) -> io::Result<bool> {
    if !pid_alive_with_ticks(pid, ticks) {
        return Ok(false);
    }
    if unsafe { libc::kill(-pgid, signal) } == 0 {
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        Ok(true)
    } else {
        Err(error)
    }
}

fn wait_for_process_to_disappear(pid: u32, ticks: u64, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if !pid_alive_with_ticks(pid, ticks) {
            return true;
        }
        let remaining = match deadline.checked_duration_since(Instant::now()) {
            Some(remaining) => remaining,
            None => return false,
        };
        thread::sleep(remaining.min(Duration::from_millis(50)));
    }
}
