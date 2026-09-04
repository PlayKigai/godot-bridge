//! Private files and directories, and the runtime directory that holds them.
//!
//! Windows has no `O_NOFOLLOW`, so every open here first checks
//! `symlink_metadata` for a reparse point and then passes
//! `FILE_FLAG_OPEN_REPARSE_POINT` so that a link created between the check and
//! the open is opened as the link itself instead of being followed.

use std::io;
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use windows_sys::Win32::Storage::FileSystem::{
    CreateDirectoryW, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_OPEN_REPARSE_POINT,
};

use super::security::{self, ADMINISTRATORS_SID, LOCAL_SYSTEM_SID};
use crate::sys::OpenMode;

/// An antivirus scanner can hold a file that was just written open for a
/// moment. One retry after this pause turns that into a slow read instead of a
/// spurious failure.
const SHARING_RETRY: Duration = Duration::from_millis(50);
/// `ERROR_SHARING_VIOLATION`, the code an antivirus scanner produces.
const ERROR_SHARING_VIOLATION: i32 = 32;

/// Open an existing file for reading, refusing to follow a reparse point.
pub fn open_nofollow_read(path: &Path) -> io::Result<std::fs::File> {
    match open_read_once(path) {
        Err(error) if is_transient(&error) => {
            std::thread::sleep(SHARING_RETRY);
            open_read_once(path)
        }
        other => other,
    }
}

fn open_read_once(path: &Path) -> io::Result<std::fs::File> {
    reject_reparse_point(path)?;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
}

fn is_transient(error: &io::Error) -> bool {
    error.raw_os_error() == Some(ERROR_SHARING_VIOLATION)
}

/// Open a file only the current user can read, refusing to follow a reparse
/// point. The runtime directory carries an inheritable access control entry
/// for the current user alone, which is what the new file picks up.
pub fn open_private(path: &Path, mode: OpenMode) -> io::Result<std::fs::File> {
    reject_reparse_point(path)?;
    let mut options = std::fs::OpenOptions::new();
    match mode {
        OpenMode::Create => options.create(true).truncate(false).read(true).write(true),
        OpenMode::Truncate => options.create(true).truncate(true).write(true),
        OpenMode::Append => options.create(true).append(true),
    };
    options
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
}

fn reject_reparse_point(path: &Path) -> io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if is_reparse_point(&metadata) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is a symlink or junction", path.display()),
        )),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn is_reparse_point(metadata: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    metadata.file_type().is_symlink()
        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

/// Create the directory if needed, with a protected DACL that names the
/// current user alone, and check that it is a real directory rather than a
/// reparse point pointing somewhere else. An existing directory is checked
/// against that same DACL and refused, never repaired, when it grants anyone
/// but the current user, `SYSTEM` or the administrators of the machine. This
/// is the Windows spelling of the Unix owner and 0700 check.
fn ensure_private_dir(path: &Path) -> io::Result<PathBuf> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => create_private_dir(path)?,
        Err(error) => return Err(error),
    }
    let metadata = std::fs::symlink_metadata(path)?;
    if is_reparse_point(&metadata) || !metadata.is_dir() {
        return Err(io::Error::other(format!(
            "runtime directory {} is not a directory without symlinks",
            path.display()
        )));
    }
    validate_private_dir(path)?;
    Ok(path.to_path_buf())
}

/// `OICI` makes the entry the default of everything created inside, so files
/// in the runtime directory stay as private as the directory itself.
fn create_private_dir(path: &Path) -> io::Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)?;
    }
    let descriptor = security::descriptor(&format!(
        "D:P(A;OICI;FA;;;{})",
        security::current_user_sid()?
    ))?;
    let attributes = descriptor.attributes();
    let name = security::wide(path.as_os_str());
    if unsafe { CreateDirectoryW(name.as_ptr(), &attributes) } == 0 {
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::AlreadyExists {
            return Err(error);
        }
    }
    Ok(())
}

fn validate_private_dir(path: &Path) -> io::Result<()> {
    let user = security::current_user_sid()?;
    let (owner, allowed) = security::file_access(path)?;
    let trusted =
        |sid: &String| *sid == user || sid == LOCAL_SYSTEM_SID || sid == ADMINISTRATORS_SID;
    if !trusted(&owner) {
        return Err(io::Error::other(format!(
            "runtime directory {} is owned by {owner} rather than the current user",
            path.display()
        )));
    }
    if let Some(stranger) = allowed.iter().find(|sid| !trusted(sid)) {
        return Err(io::Error::other(format!(
            "runtime directory {} grants access to {stranger}; \
             remove that access control entry or choose another directory",
            path.display()
        )));
    }
    Ok(())
}

/// The per-user directory that holds locks and state files.
pub fn runtime_dir() -> io::Result<PathBuf> {
    match std::env::var_os("LOCALAPPDATA") {
        Some(dir) if !dir.is_empty() => {
            ensure_private_dir(&PathBuf::from(dir).join("godot-bridge"))
        }
        _ => fallback_runtime_dir(),
    }
}

/// The runtime directory used when `%LOCALAPPDATA%` is not set.
pub fn fallback_runtime_dir() -> io::Result<PathBuf> {
    ensure_private_dir(&std::env::temp_dir().join("godot-bridge"))
}

/// The rendezvous address for one project: a named pipe, which lives in the
/// kernel object namespace rather than in the runtime directory. The runtime
/// directory is returned unchanged because Windows has no path length limit
/// that would force a fallback.
pub fn socket_path(runtime: &Path, hash: &str) -> io::Result<(PathBuf, PathBuf)> {
    Ok((runtime.to_path_buf(), pipe_name(hash)))
}

/// The rendezvous address that belongs to a state file in the runtime
/// directory, used by `status` to reach an owner it did not start. The pipe
/// name is derived from the same hash that names the state file.
pub fn socket_path_for_state(state: &Path) -> PathBuf {
    let hash = state
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default();
    pipe_name(&hash)
}

fn pipe_name(hash: &str) -> PathBuf {
    PathBuf::from(format!(r"\\.\pipe\godot-bridge-{hash}"))
}

/// Delete a socket left behind by a dead owner. A named pipe disappears with
/// the process that created it, so there is nothing to delete.
pub fn remove_socket(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::temp::TempDir;
    use std::io::{Read, Write};

    #[test]
    fn private_modes_create_truncate_and_append() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("log");

        open_private(&path, OpenMode::Append)
            .unwrap()
            .write_all(b"one\n")
            .unwrap();
        open_private(&path, OpenMode::Append)
            .unwrap()
            .write_all(b"two\n")
            .unwrap();
        assert_eq!(read(&path), "one\ntwo\n");

        open_private(&path, OpenMode::Create).unwrap();
        assert_eq!(read(&path), "one\ntwo\n");

        open_private(&path, OpenMode::Truncate)
            .unwrap()
            .write_all(b"three\n")
            .unwrap();
        assert_eq!(read(&path), "three\n");
    }

    fn read(path: &Path) -> String {
        let mut text = String::new();
        open_nofollow_read(path)
            .unwrap()
            .read_to_string(&mut text)
            .unwrap();
        text
    }

    #[test]
    fn a_junction_is_refused() {
        let dir = TempDir::new().unwrap();
        let target = dir.path().join("target");
        std::fs::create_dir(&target).unwrap();
        let link = dir.path().join("link");
        // A directory junction needs no privilege, unlike a symlink.
        let made = std::process::Command::new("cmd")
            .arg("/c")
            .arg("mklink")
            .arg("/J")
            .arg(&link)
            .arg(&target)
            .output()
            .expect("mklink runs");
        assert!(
            made.status.success(),
            "mklink /J failed: {} {}",
            String::from_utf8_lossy(&made.stdout),
            String::from_utf8_lossy(&made.stderr)
        );
        let error = ensure_private_dir(&link).unwrap_err();
        assert!(error.to_string().contains("without symlinks"), "{error}");
        assert!(open_nofollow_read(&link).is_err());
    }

    #[test]
    fn missing_file_reads_as_not_found() {
        let dir = TempDir::new().unwrap();
        let error = open_nofollow_read(&dir.path().join("absent")).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn runtime_directory_is_created_once() {
        let dir = TempDir::new().unwrap();
        let nested = dir.path().join("a").join("b");
        assert_eq!(ensure_private_dir(&nested).unwrap(), nested);
        assert_eq!(ensure_private_dir(&nested).unwrap(), nested);
        assert!(nested.is_dir());
    }

    #[test]
    fn runtime_dir_is_created_private() {
        let dir = TempDir::new().unwrap();
        let runtime = dir.path().join("runtime");
        ensure_private_dir(&runtime).unwrap();

        let user = security::current_user_sid().unwrap();
        let (owner, allowed) = security::file_access(&runtime).unwrap();
        assert!(
            owner == user || owner == LOCAL_SYSTEM_SID || owner == ADMINISTRATORS_SID,
            "owner {owner}"
        );
        assert_eq!(
            allowed,
            vec![user],
            "the runtime directory grants a stranger"
        );
    }

    #[test]
    fn permissive_runtime_dir_is_refused() {
        let dir = TempDir::new().unwrap();
        let runtime = dir.path().join("runtime");
        ensure_private_dir(&runtime).unwrap();
        let granted = std::process::Command::new("icacls")
            .arg(&runtime)
            .arg("/grant")
            .arg("*S-1-1-0:R")
            .output();
        let Ok(granted) = granted else {
            println!("icacls did not run; skipping");
            return;
        };
        if !granted.status.success() {
            println!(
                "icacls failed, skipping: {} {}",
                String::from_utf8_lossy(&granted.stdout),
                String::from_utf8_lossy(&granted.stderr)
            );
            return;
        }
        let error = ensure_private_dir(&runtime).unwrap_err();
        assert!(error.to_string().contains("S-1-1-0"), "{error}");
    }
}
