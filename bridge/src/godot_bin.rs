//! Godot binary resolution and validation.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

const VERSION_TIMEOUT: Duration = Duration::from_secs(5);
const NO_BINARY: &str =
    "No Godot binary. Set lsp.godot.settings.godot_path, or GODOT, or put godot on PATH.";
const FORBIDDEN_ARGS: [&str; 10] = [
    "--path",
    "--editor",
    "-e",
    "--headless",
    "--lsp-port",
    "--dap-port",
    "--display-driver",
    "--audio-driver",
    "--quit",
    "--quit-after",
];

/// Resolves the Godot binary: the `godot_path` setting, then the `GODOT` env
/// var, then `godot4` and `godot` on PATH. A configured value is returned
/// without an existence check; `check_version` validates it.
pub fn resolve_godot(configured: Option<&Path>) -> Result<PathBuf, String> {
    if let Some(path) = configured {
        if !path.as_os_str().is_empty() {
            return Ok(path.to_path_buf());
        }
    }
    if let Some(godot) = std::env::var_os("GODOT") {
        if !godot.is_empty() {
            return Ok(PathBuf::from(godot));
        }
    }
    for name in ["godot4", "godot"] {
        if let Some(found) = find_on_path(name) {
            return Ok(found);
        }
    }
    Err(NO_BINARY.to_string())
}

fn find_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable(candidate))
}

/// Runs `<bin> --version` with a 5 s timeout and returns its output when it
/// starts with `4.`; any other output yields the SPEC error message.
pub fn check_version(bin: &Path) -> Result<String, String> {
    let child = Command::new(bin)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("cannot run {}: {error}", bin.display()))?;
    let pid = child.id();
    let (sender, receiver) = mpsc::channel();
    let waiter = std::thread::spawn(move || {
        let _ = sender.send(child.wait_with_output());
    });
    let output = match receiver.recv_timeout(VERSION_TIMEOUT) {
        Ok(output) => output,
        Err(mpsc::RecvTimeoutError::Timeout) => {
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGKILL);
            }
            let _ = waiter.join();
            return Err(format!(
                "Godot at {} did not answer --version within {} s",
                bin.display(),
                VERSION_TIMEOUT.as_secs()
            ));
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            return Err(format!(
                "cannot run {}: --version check failed",
                bin.display()
            ));
        }
    }
    .map_err(|error| format!("cannot run {}: {error}", bin.display()))?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
    .trim()
    .to_string();
    if text.starts_with("4.") {
        Ok(text)
    } else {
        Err(format!("Godot at {} is not 4.x: {text}", bin.display()))
    }
}

/// Rejects any `extra_args` element equal to a forbidden Godot flag, and any
/// long forbidden flag in `--flag=value` form.
pub fn validate_extra_args(args: &[String]) -> Result<(), String> {
    for arg in args {
        for flag in FORBIDDEN_ARGS {
            let rejected = if flag.starts_with("--") {
                arg == flag
                    || arg
                        .strip_prefix(flag)
                        .is_some_and(|rest| rest.starts_with('='))
            } else {
                arg == flag
            };
            if rejected {
                return Err(format!(
                    "extra_args element {arg:?} uses disallowed flag {flag:?}"
                ));
            }
        }
    }
    Ok(())
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.is_file()
        && std::fs::metadata(path).is_ok_and(|meta| meta.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn write_script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, body).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn touch(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, "").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[test]
    fn version_4x_is_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let bin = write_script(dir.path(), "godot", "#!/bin/sh\necho 4.7.2.stable\n");
        let version = check_version(&bin).unwrap();
        assert!(version.starts_with("4."));
        assert_eq!(version, "4.7.2.stable");
    }

    #[test]
    fn version_3x_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let bin = write_script(dir.path(), "godot", "#!/bin/sh\necho 3.5\n");
        let error = check_version(&bin).unwrap_err();
        assert_eq!(error, format!("Godot at {} is not 4.x: 3.5", bin.display()));
    }

    #[test]
    fn version_timeout_is_rejected() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let sleeper = if Path::new("/bin/sleep").exists() {
            "/bin/sleep"
        } else {
            "/usr/bin/sleep"
        };
        let bin = write_script(
            dir.path(),
            "godot",
            &format!("#!/bin/sh\nexec {sleeper} 10\n"),
        );
        let error = check_version(&bin).unwrap_err();
        assert!(error.contains("did not answer --version"), "{error}");
    }

    #[test]
    fn resolve_precedence() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let setting = touch(dir.path(), "setting");
        let env_bin = touch(dir.path(), "env-bin");
        let first = dir.path().join("first");
        let second = dir.path().join("second");
        let empty = dir.path().join("empty");
        for path in [&first, &second, &empty] {
            fs::create_dir_all(path).unwrap();
        }
        touch(&first, "godot4");
        touch(&first, "godot");
        touch(&second, "godot");

        let old_path = env::var_os("PATH");
        let old_godot = env::var_os("GODOT");
        env::set_var("PATH", format!("{}:{}", first.display(), second.display()));

        env::set_var("GODOT", &env_bin);
        assert_eq!(resolve_godot(Some(&setting)).unwrap(), setting);

        assert_eq!(resolve_godot(None).unwrap(), env_bin);

        env::set_var("GODOT", "");
        assert_eq!(resolve_godot(None).unwrap(), first.join("godot4"));

        env::remove_var("GODOT");
        env::set_var("PATH", &empty);
        let error = resolve_godot(None).unwrap_err();
        assert_eq!(error, NO_BINARY);

        match old_path {
            Some(value) => env::set_var("PATH", value),
            None => env::remove_var("PATH"),
        }
        match old_godot {
            Some(value) => env::set_var("GODOT", value),
            None => env::remove_var("GODOT"),
        }
    }

    #[test]
    fn extra_args_accepts_safe_flags() {
        let cases: [&[String]; 4] = [
            &[],
            &[String::from("--foo")],
            &[String::from("--foo=bar")],
            &[String::from("-x"), String::from("value")],
        ];
        for args in cases {
            validate_extra_args(args).unwrap();
        }
    }

    #[test]
    fn extra_args_rejects_each_flag() {
        for flag in FORBIDDEN_ARGS {
            let args = [flag.to_string()];
            let error = validate_extra_args(&args).unwrap_err();
            assert!(error.contains(flag), "{error}");
        }
    }

    #[test]
    fn extra_args_rejects_flag_equals_value() {
        for flag in FORBIDDEN_ARGS {
            if !flag.starts_with("--") {
                continue;
            }
            let args = [format!("{flag}=value")];
            let error = validate_extra_args(&args).unwrap_err();
            assert!(error.contains(flag), "{error}");
        }
    }
}
