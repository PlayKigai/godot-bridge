//! Process identity, spawning and teardown.
//!
//! Identity is real: the creation `FILETIME` of a process distinguishes a
//! reused pid exactly as `/proc/<pid>/stat` field 22 does on Linux. A headless
//! child is spawned suspended, assigned to a job object with
//! `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` and only then resumed, so its whole
//! tree dies with the bridge the way `PDEATHSIG` and a process group do on
//! Linux. The GUI editor breaks away from any job the bridge itself sits in,
//! because it has to survive the hand-off, and is then assigned to a named job
//! of its own without `KILL_ON_JOB_CLOSE`; a later bridge reopens that job by
//! a name it derives from the state file and terminates the whole tree, games
//! the editor launched included.
//!
//! Nothing is terminated through a bare pid. A target is opened once, its
//! creation time is compared on that handle, and the same handle carries the
//! `TerminateProcess` and the wait, so a pid reused in between cannot be the
//! process that dies.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::io;
use std::os::windows::io::{AsHandle, AsRawHandle, BorrowedHandle, FromRawHandle, OwnedHandle};
use std::os::windows::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use windows_sys::Win32::Foundation::{
    DuplicateHandle, ERROR_ACCESS_DENIED, ERROR_INSUFFICIENT_BUFFER, ERROR_INVALID_PARAMETER,
    FILETIME, HANDLE, INVALID_HANDLE_VALUE, NO_ERROR, STILL_ACTIVE, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::NetworkManagement::IpHelper::{
    GetExtendedTcpTable, MIB_TCP6ROW_OWNER_PID, MIB_TCP6TABLE_OWNER_PID, MIB_TCPROW_OWNER_PID,
    MIB_TCPTABLE_OWNER_PID, MIB_TCP_STATE_LISTEN, TCP_TABLE_OWNER_PID_LISTENER,
};
use windows_sys::Win32::Networking::WinSock::{ADDRESS_FAMILY, AF_INET, AF_INET6};
use windows_sys::Win32::Storage::FileSystem::SYNCHRONIZE;
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, Thread32First, Thread32Next,
    CREATE_TOOLHELP_SNAPSHOT_FLAGS, PROCESSENTRY32W, TH32CS_SNAPPROCESS, TH32CS_SNAPTHREAD,
    THREADENTRY32,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation, OpenJobObjectW,
    SetInformationJobObject, TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetExitCodeProcess, GetProcessTimes, OpenProcess, OpenThread, ResumeThread,
    TerminateProcess, WaitForSingleObject, CREATE_BREAKAWAY_FROM_JOB, CREATE_NO_WINDOW,
    CREATE_SUSPENDED, PROCESS_ACCESS_RIGHTS, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE,
    THREAD_SUSPEND_RESUME,
};

use super::security;

const GROUP_WAIT: Duration = Duration::from_secs(5);
/// The idle process, the system process and its earliest children are never
/// targets, and neither is the bridge itself.
const LOWEST_KILLABLE_PID: u32 = 5;
const TABLE_WORDS: usize = 1024;
const TABLE_ATTEMPTS: usize = 8;
/// `JOB_OBJECT_QUERY` and `JOB_OBJECT_TERMINATE`, which live in a
/// `windows-sys` feature this crate does not otherwise need.
const JOB_OBJECT_QUERY: u32 = 0x4;
const JOB_OBJECT_TERMINATE: u32 = 0x8;

/// The job of every child this process spawned, headless or GUI. Closing the
/// last handle to a `KILL_ON_JOB_CLOSE` job kills its processes, so the handle
/// has to outlive the child and the `(Child, pgid, ticks)` triple cannot carry
/// it; the GUI job carries no such limit and only has to stay open while the
/// bridge lives.
static JOBS: Mutex<BTreeMap<u32, OwnedHandle>> = Mutex::new(BTreeMap::new());

/// A value that changes when a pid is reused, so a recorded pid can be checked
/// before it is terminated. Windows uses the creation `FILETIME`, a count of
/// 100 ns intervals since 1601.
pub fn process_start_ticks(pid: u32) -> io::Result<u64> {
    let process = open_process(PROCESS_QUERY_LIMITED_INFORMATION, pid)?;
    creation_ticks(&process)
}

/// Whether the process recorded as `pid` with `ticks` is still that same
/// process, so a reused pid is never mistaken for the original.
pub fn pid_alive_with_ticks(pid: u32, ticks: u64) -> bool {
    process_start_ticks(pid).ok() == Some(ticks)
}

/// Whether `pid` owns the listening socket on `port`, so the bridge never
/// talks to a stranger that grabbed the port first.
pub fn port_listener_belongs_to_process(pid: u32, port: u16) -> io::Result<bool> {
    if ipv4_listener_owner(pid, port)? {
        return Ok(true);
    }
    match ipv6_listener_owner(pid, port) {
        Ok(owner) => Ok(owner),
        Err(error) => {
            // A host with the IPv6 stack disabled has no second table to read,
            // and the IPv4 answer above already stands.
            crate::warn!("cannot read the IPv6 listening table: {error}");
            Ok(false)
        }
    }
}

/// Spawn the headless editor into a job object that dies with the bridge, so
/// the whole Godot tree goes with it. Returns the child, its group id, which
/// is the pid that names the job, and its identity.
pub fn spawn_headless_process(command: &mut Command) -> io::Result<(Child, u32, u64)> {
    let job = create_kill_on_close_job()?;
    command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_SUSPENDED | CREATE_NO_WINDOW);
    let (mut child, pid, ticks) = spawn_with_identity(command)?;
    match assign_to_job_and_resume(&child, &job, pid) {
        Ok(()) => {
            jobs().insert(pid, job);
            Ok((child, pid, ticks))
        }
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            Err(error)
        }
    }
}

/// Spawn the GUI editor with both output streams going to `log`. It leaves the
/// job the bridge itself may sit in, because it must outlive the bridge, and
/// joins a named job of its own that has no `KILL_ON_JOB_CLOSE`, so that a
/// later bridge can still take down the whole tree. It is spawned suspended
/// like the headless path, so the assignment precedes any child of its own.
pub fn spawn_gui_process(
    command: &mut Command,
    log: std::fs::File,
) -> io::Result<(Child, u32, u64)> {
    let error_output = log.try_clone()?;
    command
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(error_output))
        .creation_flags(CREATE_NO_WINDOW | CREATE_BREAKAWAY_FROM_JOB | CREATE_SUSPENDED);
    let (mut child, pid, ticks) = match spawn_with_identity(command) {
        Err(error) if error.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) => {
            crate::warn!("the editor cannot leave the job of the bridge: {error}");
            command.creation_flags(CREATE_NO_WINDOW | CREATE_SUSPENDED);
            spawn_with_identity(command)?
        }
        other => other?,
    };
    match create_gui_job(&child, pid, ticks) {
        Ok(job) => {
            jobs().insert(pid, job);
        }
        // Windows 8 and later allow nested jobs; an older one that forbids
        // them leaves the editor where it is, and the descendant walk stands in.
        Err(error) => crate::warn!("the editor runs outside a job object: {error}"),
    }
    if let Err(error) = resume_process(pid) {
        jobs().remove(&pid);
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
    Ok((child, pid, ticks))
}

/// Terminate a child the bridge still owns, with its whole tree when a job
/// holds it. The caller reaps the child afterwards.
pub fn kill_group(child: &mut Child, pid: u32, pgid: u32, ticks: u64) -> io::Result<()> {
    validate_ids(pid, pgid)?;
    let job = jobs().remove(&pid);
    if !pid_alive_with_ticks(pid, ticks) {
        return Ok(());
    }
    match &job {
        Some(job) => terminate_job(job)?,
        None => child.kill()?,
    }
    if !wait_for_exit(child.as_handle(), GROUP_WAIT)? {
        crate::warn!("process {pid} did not exit within {GROUP_WAIT:?}");
    }
    Ok(())
}

/// Terminate a process recorded in a state file, which this process may never
/// have spawned, together with everything it started. A GUI editor carries a
/// named job that takes its whole tree at once; without one, every descendant
/// is opened before any of them is terminated and killed individually.
pub fn kill_recorded(pid: u32, pgid: u32, ticks: u64) -> io::Result<()> {
    validate_ids(pid, pgid)?;
    let Some(leader) = Target::open(pid)? else {
        return Ok(());
    };
    if leader.ticks != ticks {
        return Ok(());
    }
    if let Some(job) = open_gui_job(pid, ticks) {
        terminate_job(&job)?;
        return leader.wait();
    }
    let descendants = descendants_of(&leader)?;
    leader.terminate()?;
    for descendant in descendants {
        if let Err(error) = descendant.terminate() {
            crate::warn!(
                "cannot terminate descendant {} of process {pid}: {error}",
                descendant.pid
            );
        }
    }
    Ok(())
}

fn ipv4_listener_owner(pid: u32, port: u16) -> io::Result<bool> {
    let table = tcp_listener_table(AF_INET)?;
    let rows: &[MIB_TCPROW_OWNER_PID] =
        table_rows(&table, std::mem::offset_of!(MIB_TCPTABLE_OWNER_PID, table));
    Ok(rows
        .iter()
        .any(|row| row.dwOwningPid == pid && listens_on(row.dwState, row.dwLocalPort, port)))
}

fn ipv6_listener_owner(pid: u32, port: u16) -> io::Result<bool> {
    let table = tcp_listener_table(AF_INET6)?;
    let rows: &[MIB_TCP6ROW_OWNER_PID] =
        table_rows(&table, std::mem::offset_of!(MIB_TCP6TABLE_OWNER_PID, table));
    Ok(rows
        .iter()
        .any(|row| row.dwOwningPid == pid && listens_on(row.dwState, row.dwLocalPort, port)))
}

/// `dwLocalPort` carries the port in network byte order in its low 16 bits.
fn listens_on(state: u32, local_port: u32, port: u16) -> bool {
    state == MIB_TCP_STATE_LISTEN as u32 && u16::from_be(local_port as u16) == port
}

/// The listening table of one address family, read into words so that the
/// buffer is aligned for the row structures the kernel writes into it.
fn tcp_listener_table(family: ADDRESS_FAMILY) -> io::Result<Vec<u32>> {
    let mut table = vec![0u32; TABLE_WORDS];
    for _ in 0..TABLE_ATTEMPTS {
        let mut size = byte_len(&table);
        let status = unsafe {
            GetExtendedTcpTable(
                table.as_mut_ptr().cast(),
                &mut size,
                0,
                u32::from(family),
                TCP_TABLE_OWNER_PID_LISTENER,
                0,
            )
        };
        if status == NO_ERROR {
            return Ok(table);
        }
        if status != ERROR_INSUFFICIENT_BUFFER {
            return Err(io::Error::from_raw_os_error(status as i32));
        }
        table = vec![0u32; words_for(size)];
    }
    Err(io::Error::other(
        "the listening port table grew faster than it could be read",
    ))
}

/// The rows of an `..._OWNER_PID` table, whose leading `u32` is `dwNumEntries`
/// and whose row array starts at `offset`. A count larger than the buffer is
/// clamped, so the slice never leaves the allocation.
fn table_rows<T>(buffer: &[u32], offset: usize) -> &[T] {
    let Some(available) = std::mem::size_of_val(buffer).checked_sub(offset) else {
        return &[];
    };
    let count = buffer.first().copied().unwrap_or(0) as usize;
    let rows = count.min(available / std::mem::size_of::<T>());
    unsafe { std::slice::from_raw_parts(buffer.as_ptr().byte_add(offset).cast::<T>(), rows) }
}

fn byte_len(words: &[u32]) -> u32 {
    u32::try_from(std::mem::size_of_val(words)).unwrap_or(u32::MAX)
}

fn words_for(bytes: u32) -> usize {
    (bytes as usize)
        .div_ceil(std::mem::size_of::<u32>())
        .max(TABLE_WORDS)
}

fn create_kill_on_close_job() -> io::Result<OwnedHandle> {
    let job = owned(unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) })?;
    let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    let set = unsafe {
        SetInformationJobObject(
            job.as_raw_handle(),
            JobObjectExtendedLimitInformation,
            std::ptr::from_ref(&limits).cast(),
            std::mem::size_of_val(&limits) as u32,
        )
    };
    if set == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(job)
}

fn assign_to_job_and_resume(child: &Child, job: &OwnedHandle, pid: u32) -> io::Result<()> {
    let assigned = unsafe { AssignProcessToJobObject(job.as_raw_handle(), child.as_raw_handle()) };
    if assigned == 0 {
        return Err(io::Error::last_os_error());
    }
    resume_process(pid)
}

/// Start the threads of a process spawned suspended. The caller holds the
/// child handle, so the pid cannot be reused and every thread the snapshot
/// attributes to it really belongs to that child.
fn resume_process(pid: u32) -> io::Result<()> {
    let snapshot = take_snapshot(TH32CS_SNAPTHREAD)?;
    let mut entry = THREADENTRY32 {
        dwSize: std::mem::size_of::<THREADENTRY32>() as u32,
        ..Default::default()
    };
    let mut more = unsafe { Thread32First(snapshot.as_raw_handle(), &mut entry) };
    let mut resumed = false;
    while more != 0 {
        if entry.th32OwnerProcessID == pid {
            resume_thread(entry.th32ThreadID)?;
            resumed = true;
        }
        more = unsafe { Thread32Next(snapshot.as_raw_handle(), &mut entry) };
    }
    if !resumed {
        return Err(io::Error::other(format!(
            "process {pid} has no thread to resume"
        )));
    }
    Ok(())
}

fn resume_thread(id: u32) -> io::Result<()> {
    let thread = owned(unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, id) })?;
    if unsafe { ResumeThread(thread.as_raw_handle()) } == u32::MAX {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// A process held open for the whole verify, terminate and wait sequence, so
/// that a pid reused between the check and the kill cannot be what dies.
struct Target {
    pid: u32,
    ticks: u64,
    process: OwnedHandle,
}

impl Target {
    /// `Ok(None)` means the pid names no process at all.
    fn open(pid: u32) -> io::Result<Option<Self>> {
        if !is_killable(pid) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("process {pid} must not be terminated"),
            ));
        }
        let access = PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE | SYNCHRONIZE;
        let process = match open_process(access, pid) {
            Ok(process) => process,
            // A pid that no longer names a process cannot be opened, and needs
            // no terminating; this is the Windows spelling of `ESRCH`.
            Err(error) if error.raw_os_error() == Some(ERROR_INVALID_PARAMETER as i32) => {
                return Ok(None)
            }
            Err(error) => return Err(error),
        };
        let ticks = creation_ticks(&process)?;
        Ok(Some(Self {
            pid,
            ticks,
            process,
        }))
    }

    fn terminate(&self) -> io::Result<()> {
        if unsafe { TerminateProcess(self.process.as_raw_handle(), 1) } == 0 {
            let error = io::Error::last_os_error();
            if !has_exited(&self.process)? {
                return Err(error);
            }
            return Ok(());
        }
        self.wait()
    }

    fn wait(&self) -> io::Result<()> {
        if !wait_for_exit(self.process.as_handle(), GROUP_WAIT)? {
            crate::warn!("process {} did not exit within {GROUP_WAIT:?}", self.pid);
        }
        Ok(())
    }
}

/// Everything the leader started, and everything those started, which a job
/// object would have taken in one call. Every handle is opened before any of
/// them is terminated, so no pid in the walk can be reused underneath it. A
/// pid whose parent field names one of them but that started before its parent
/// inherited that field from an older process with the same pid.
fn descendants_of(leader: &Target) -> io::Result<Vec<Target>> {
    let tree = process_tree()?;
    let mut descendants: Vec<Target> = Vec::new();
    let mut frontier = vec![(leader.pid, leader.ticks)];
    while let Some((parent, since)) = frontier.pop() {
        for (pid, _) in tree.iter().copied().filter(|(_, ppid)| *ppid == parent) {
            if !is_killable(pid) || descendants.iter().any(|target| target.pid == pid) {
                continue;
            }
            let Ok(Some(target)) = Target::open(pid) else {
                continue;
            };
            if target.ticks < since {
                continue;
            }
            frontier.push((pid, target.ticks));
            descendants.push(target);
        }
    }
    Ok(descendants)
}

/// Every live process as `(pid, parent pid)`, read once so the walk sees one
/// consistent tree.
fn process_tree() -> io::Result<Vec<(u32, u32)>> {
    let snapshot = take_snapshot(TH32CS_SNAPPROCESS)?;
    let mut entry = PROCESSENTRY32W {
        dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    let mut more = unsafe { Process32FirstW(snapshot.as_raw_handle(), &mut entry) };
    let mut tree = Vec::new();
    while more != 0 {
        tree.push((entry.th32ProcessID, entry.th32ParentProcessID));
        more = unsafe { Process32NextW(snapshot.as_raw_handle(), &mut entry) };
    }
    Ok(tree)
}

/// The name of the GUI job, derived from exactly what a state file records, so
/// a bridge that never spawned the editor can still find it.
fn gui_job_name(pid: u32, ticks: u64) -> String {
    format!(r"Local\godot-bridge-{pid}-{ticks}")
}

/// A named job for the GUI editor, admitting the current user only and without
/// `KILL_ON_JOB_CLOSE`: closing the handle must not take the editor with it.
fn create_gui_job(child: &Child, pid: u32, ticks: u64) -> io::Result<OwnedHandle> {
    let descriptor = security::current_user_descriptor()?;
    let attributes = descriptor.attributes();
    let name = security::wide(OsStr::new(&gui_job_name(pid, ticks)));
    let job = owned(unsafe { CreateJobObjectW(&attributes, name.as_ptr()) })?;
    if unsafe { AssignProcessToJobObject(job.as_raw_handle(), child.as_raw_handle()) } == 0 {
        return Err(io::Error::last_os_error());
    }
    lend_job_to_editor(&job, child)?;
    Ok(job)
}

/// Windows unlinks a named object from the namespace as soon as its last
/// handle closes, whatever is still running inside it, so the editor is given
/// a handle of its own: the name then lasts exactly as long as the editor,
/// which is how a later bridge finds the job at all.
fn lend_job_to_editor(job: &OwnedHandle, child: &Child) -> io::Result<()> {
    let mut lent: HANDLE = std::ptr::null_mut();
    let duplicated = unsafe {
        DuplicateHandle(
            GetCurrentProcess(),
            job.as_raw_handle(),
            child.as_raw_handle(),
            &mut lent,
            JOB_OBJECT_QUERY,
            0,
            0,
        )
    };
    if duplicated == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// `None` means no such job: an editor from a bridge that did not make one, or
/// one whose assignment failed.
fn open_gui_job(pid: u32, ticks: u64) -> Option<OwnedHandle> {
    let name = security::wide(OsStr::new(&gui_job_name(pid, ticks)));
    let job = unsafe { OpenJobObjectW(JOB_OBJECT_TERMINATE | JOB_OBJECT_QUERY, 0, name.as_ptr()) };
    owned(job).ok()
}

fn terminate_job(job: &OwnedHandle) -> io::Result<()> {
    if unsafe { TerminateJobObject(job.as_raw_handle(), 1) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn is_killable(pid: u32) -> bool {
    pid >= LOWEST_KILLABLE_PID && pid != std::process::id()
}

fn has_exited(process: &OwnedHandle) -> io::Result<bool> {
    let mut code = 0u32;
    if unsafe { GetExitCodeProcess(process.as_raw_handle(), &mut code) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(code != STILL_ACTIVE as u32)
}

fn wait_for_exit(process: BorrowedHandle<'_>, timeout: Duration) -> io::Result<bool> {
    let millis = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX);
    match unsafe { WaitForSingleObject(process.as_raw_handle(), millis) } {
        WAIT_OBJECT_0 => Ok(true),
        WAIT_TIMEOUT => Ok(false),
        _ => Err(io::Error::last_os_error()),
    }
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

fn creation_ticks(process: &OwnedHandle) -> io::Result<u64> {
    let mut creation = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let mut exit = creation;
    let mut kernel = creation;
    let mut user = creation;
    let ok = unsafe {
        GetProcessTimes(
            process.as_raw_handle(),
            &mut creation,
            &mut exit,
            &mut kernel,
            &mut user,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(u64::from(creation.dwHighDateTime) << 32 | u64::from(creation.dwLowDateTime))
}

fn open_process(access: PROCESS_ACCESS_RIGHTS, pid: u32) -> io::Result<OwnedHandle> {
    owned(unsafe { OpenProcess(access, 0, pid) })
}

fn take_snapshot(flags: CREATE_TOOLHELP_SNAPSHOT_FLAGS) -> io::Result<OwnedHandle> {
    owned(unsafe { CreateToolhelp32Snapshot(flags, 0) })
}

/// Take ownership of a handle the call just opened, or report why it opened
/// none.
fn owned(handle: HANDLE) -> io::Result<OwnedHandle> {
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
}

fn jobs() -> MutexGuard<'static, BTreeMap<u32, OwnedHandle>> {
    JOBS.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// On Windows the process group id recorded in the state file is the pid of
/// the leader, so the same invariant as on Unix holds.
fn validate_ids(pid: u32, pgid: u32) -> io::Result<()> {
    if pid <= 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "process id must be greater than 1",
        ));
    }
    if pgid <= 1 || pgid != pid {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "process group id must be greater than 1 and match the leader",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::temp::TempDir;
    use std::net::TcpListener;
    use std::thread;
    use std::time::Instant;

    const TREE: [&str; 2] = ["/c", "ping -n 30 127.0.0.1 >nul"];
    /// One more `cmd.exe` than [`TREE`], so [`GRANDCHILD`] really is one.
    const DEEP_TREE: [&str; 2] = ["/c", "cmd.exe /c ping -n 30 127.0.0.1 >nul"];
    const GRANDCHILD: &str = "ping.exe";
    const SYSTEM_PID: u32 = 4;
    const APPEAR_WAIT: Duration = Duration::from_secs(30);

    #[test]
    fn listener_owner_matches_process() {
        for address in ["127.0.0.1:0", "[::1]:0"] {
            let listener = TcpListener::bind(address).expect("listener binds");
            let port = listener.local_addr().expect("local address").port();
            assert!(
                port_listener_belongs_to_process(std::process::id(), port).expect("owner table"),
                "{address} should be attributed to this process"
            );
        }
    }

    #[test]
    fn foreign_pid_does_not_own_port() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener binds");
        let port = listener.local_addr().expect("local address").port();
        assert!(!port_listener_belongs_to_process(SYSTEM_PID, port).expect("owner table"));
    }

    #[test]
    fn headless_child_dies_with_job() {
        let mut command = Command::new("cmd.exe");
        command.args(TREE);
        let (mut child, pid, ticks) = spawn_headless_process(&mut command).expect("tree spawns");
        let grandchild = wait_for_grandchild_of(pid);

        kill_group(&mut child, pid, pid, ticks).expect("the job is terminated");
        let _ = child.wait();
        assert!(
            wait_until_exited(grandchild),
            "grandchild {grandchild} should die with the job"
        );
    }

    #[test]
    fn gui_process_writes_both_streams_to_the_log() {
        let directory = TempDir::new().expect("temporary directory");
        let path = directory.path().join("gui.log");
        let log = std::fs::File::create(&path).expect("log file");
        let mut command = Command::new("cmd.exe");
        command.args(["/c", "echo out& echo err 1>&2"]);

        let (mut child, pid, ticks) = spawn_gui_process(&mut command, log).expect("editor spawns");
        assert_eq!(process_start_ticks(pid).ok(), Some(ticks));
        let _ = child.wait();

        let log = std::fs::read_to_string(&path).expect("log is readable");
        assert!(log.contains("out") && log.contains("err"), "{log}");
    }

    #[test]
    fn gui_job_kills_grandchild() {
        let directory = TempDir::new().expect("temporary directory");
        let path = directory.path().join("gui.log");
        let log = std::fs::File::create(&path).expect("log file");
        let mut command = Command::new("cmd.exe");
        command.args(DEEP_TREE);

        let (mut child, pid, ticks) = spawn_gui_process(&mut command, log).expect("editor spawns");
        let grandchild = wait_for_descendant(pid, GRANDCHILD);
        assert_eq!(
            named_child_of(pid, GRANDCHILD),
            None,
            "{GRANDCHILD} should be a grandchild, not a child"
        );

        // A later bridge holds no handle, and finds the job by its name alone.
        jobs().remove(&pid);
        assert!(
            open_gui_job(pid, ticks).is_some(),
            "the job outlives the handle the spawning bridge held"
        );

        kill_recorded(pid, pid, ticks).expect("the job is terminated");
        let _ = child.wait();
        assert!(wait_until_exited(pid), "leader {pid} should be gone");
        assert!(
            wait_until_exited(grandchild),
            "grandchild {grandchild} should die with the job"
        );
    }

    #[test]
    fn kill_recorded_without_job_falls_back() {
        let (mut child, pid, ticks) = spawn_unowned_tree(DEEP_TREE);
        let grandchild = wait_for_descendant(pid, GRANDCHILD);
        assert!(
            open_gui_job(pid, ticks).is_none(),
            "a plain spawn joins no job"
        );

        kill_recorded(pid, pid, ticks).expect("the recorded tree is terminated");
        let _ = child.wait();
        assert!(wait_until_exited(pid), "leader {pid} should be gone");
        assert!(
            wait_until_exited(grandchild),
            "grandchild {grandchild} should be gone"
        );
    }

    #[test]
    fn stale_ticks_are_not_killed() {
        let (mut child, pid, ticks) = spawn_unowned_tree(TREE);

        kill_recorded(pid, pid, ticks ^ 1).expect("a stale record is not an error");
        assert!(!process_exited(pid), "process {pid} should survive");

        kill_recorded(pid, pid, ticks).expect("the live record is terminated");
        let _ = child.wait();
    }

    fn spawn_unowned_tree(args: [&str; 2]) -> (Child, u32, u64) {
        let mut child = Command::new("cmd.exe")
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .expect("tree spawns");
        let pid = child.id();
        match process_start_ticks(pid) {
            Ok(ticks) => (child, pid, ticks),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("identity of {pid}: {error}");
            }
        }
    }

    fn wait_for_grandchild_of(pid: u32) -> u32 {
        wait_for(pid, GRANDCHILD, named_child_of)
    }

    fn wait_for_descendant(pid: u32, name: &str) -> u32 {
        wait_for(pid, name, named_descendant_of)
    }

    fn wait_for(pid: u32, name: &str, find: fn(u32, &str) -> Option<u32>) -> u32 {
        let deadline = Instant::now() + APPEAR_WAIT;
        loop {
            if let Some(found) = find(pid, name) {
                return found;
            }
            assert!(
                Instant::now() < deadline,
                "{name} under {pid} should appear"
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// The console host that Windows attaches to the tree is a child too, so
    /// the process that has to die is named rather than taken by position.
    fn named_child_of(pid: u32, name: &str) -> Option<u32> {
        snapshot_processes()
            .into_iter()
            .find(|(_, parent, executable)| *parent == pid && executable == name)
            .map(|(child, _, _)| child)
    }

    fn named_descendant_of(pid: u32, name: &str) -> Option<u32> {
        let processes = snapshot_processes();
        let mut frontier = vec![pid];
        let mut seen = vec![pid];
        while let Some(parent) = frontier.pop() {
            for (child, _, executable) in processes.iter().filter(|(_, ppid, _)| *ppid == parent) {
                if seen.contains(child) {
                    continue;
                }
                seen.push(*child);
                if executable == name {
                    return Some(*child);
                }
                frontier.push(*child);
            }
        }
        None
    }

    fn snapshot_processes() -> Vec<(u32, u32, String)> {
        let snapshot = take_snapshot(TH32CS_SNAPPROCESS).expect("process snapshot");
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut more = unsafe { Process32FirstW(snapshot.as_raw_handle(), &mut entry) };
        let mut processes = Vec::new();
        while more != 0 {
            processes.push((
                entry.th32ProcessID,
                entry.th32ParentProcessID,
                executable_name(&entry),
            ));
            more = unsafe { Process32NextW(snapshot.as_raw_handle(), &mut entry) };
        }
        processes
    }

    fn executable_name(entry: &PROCESSENTRY32W) -> String {
        let end = entry
            .szExeFile
            .iter()
            .position(|unit| *unit == 0)
            .unwrap_or(entry.szExeFile.len());
        String::from_utf16_lossy(&entry.szExeFile[..end]).to_ascii_lowercase()
    }

    fn wait_until_exited(pid: u32) -> bool {
        let deadline = Instant::now() + GROUP_WAIT;
        while Instant::now() < deadline {
            if process_exited(pid) {
                return true;
            }
            thread::sleep(Duration::from_millis(20));
        }
        process_exited(pid)
    }

    fn process_exited(pid: u32) -> bool {
        match open_process(PROCESS_QUERY_LIMITED_INFORMATION, pid) {
            Ok(process) => has_exited(&process).unwrap_or(true),
            Err(_) => true,
        }
    }
}
