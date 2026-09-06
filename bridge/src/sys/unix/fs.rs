//! Private files and directories, and the runtime directory that holds them.

use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use crate::sys::OpenMode;

/// Open an existing file for reading, refusing to follow a symlink.
pub fn open_nofollow_read(path: &Path) -> io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}

/// Open a file only the current user can read, refusing to follow a symlink.
pub fn open_private(path: &Path, mode: OpenMode) -> io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    match mode {
        OpenMode::Create => options.create(true).truncate(false).read(true).write(true),
        OpenMode::Truncate => options.create(true).truncate(true).write(true),
        OpenMode::Append => options.create(true).append(true),
    };
    options
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}

/// Create the directory if needed and check that it is a private directory
/// owned by the current user, without traversing a symlink.
fn ensure_private_dir(path: &Path) -> io::Result<PathBuf> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => validate_private_dir(path)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let mut builder = std::fs::DirBuilder::new();
            builder.mode(0o700);
            builder.create(path)?;
            validate_private_dir(path)?;
        }
        Err(error) => return Err(error),
    }
    let _directory = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(path)
        .map_err(|error| {
            io::Error::new(
                error.kind(),
                format!(
                    "cannot securely open runtime directory {}: {error}",
                    path.display()
                ),
            )
        })?;
    Ok(path.to_path_buf())
}

fn validate_private_dir(path: &Path) -> io::Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(io::Error::other(format!(
            "runtime directory {} is not a directory without symlinks",
            path.display()
        )));
    }
    if metadata.uid() != effective_uid() || metadata.mode() & 0o777 != 0o700 {
        return Err(io::Error::other(format!(
            "runtime directory {} must be owned by the current user with mode 0700",
            path.display()
        )));
    }
    Ok(())
}

/// The per-user directory that holds locks, state files and sockets.
pub fn runtime_dir() -> io::Result<PathBuf> {
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) => ensure_private_dir(&PathBuf::from(dir).join("godot-bridge")),
        None => fallback_runtime_dir(),
    }
}

/// The runtime directory used when the preferred one is missing or too deep.
pub fn fallback_runtime_dir() -> io::Result<PathBuf> {
    ensure_private_dir(&PathBuf::from(format!(
        "/tmp/godot-bridge-{}",
        effective_uid()
    )))
}

/// The rendezvous address for one project: a Unix socket in the runtime
/// directory. Falls back to the shorter `/tmp` directory when `sun_path` would
/// overflow.
pub fn socket_path(runtime: &Path, hash: &str) -> io::Result<(PathBuf, PathBuf)> {
    let socket = runtime.join(format!("{hash}.sock"));
    if socket.as_os_str().len() <= 100 {
        return Ok((runtime.to_path_buf(), socket));
    }
    crate::warn!(
        "socket path {} is too long; using fallback runtime directory",
        socket.display()
    );
    let fallback = fallback_runtime_dir()?;
    let socket = fallback.join(format!("{hash}.sock"));
    Ok((fallback, socket))
}

/// The rendezvous address that belongs to a state file in the runtime
/// directory, used by `status` to reach an owner it did not start.
pub fn socket_path_for_state(state: &Path) -> PathBuf {
    state.with_extension("sock")
}

/// Delete a socket left behind by a dead owner. Missing is success.
pub fn remove_socket(path: &Path) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

fn effective_uid() -> u32 {
    unsafe { libc::geteuid() }
}
