use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const VERSION_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(unix)]
const NO_BINARY: &str =
    "No Godot binary. Set lsp.godot.settings.godot_path, or GODOT, or put godot on PATH.";
#[cfg(windows)]
const NO_BINARY: &str =
    "No Godot binary. Set lsp.godot.settings.godot_path, or GODOT, or put godot.exe on PATH.";
const FORBIDDEN_ARGS: [&str; 16] = [
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
    "--script",
    "-s",
    "--main-pack",
    "--export-release",
    "--export-debug",
    "--export-pack",
];

pub fn resolve_godot(configured: Option<&Path>) -> Result<PathBuf, String> {
    if let Some(path) = configured {
        if !path.as_os_str().is_empty() {
            validate_godot_path(path)?;
            return Ok(path.to_path_buf());
        }
    }
    if let Some(godot) = std::env::var_os("GODOT") {
        if !godot.is_empty() {
            let path = PathBuf::from(godot);
            validate_godot_path(&path)?;
            return Ok(path);
        }
    }
    for name in ["godot4", "godot"] {
        if let Some(found) = find_on_path(name) {
            return Ok(prefer_direct(found));
        }
    }
    if let Some(found) = find_well_known() {
        return Ok(prefer_direct(found));
    }
    Err(NO_BINARY.to_string())
}

#[cfg(unix)]
fn find_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable(candidate))
}

/// On Windows a command name on `PATH` carries no extension; `PATHEXT` lists
/// the suffixes the shell appends, and a package manager shim is often a
/// `.cmd` or `.bat` rather than an `.exe`.
#[cfg(windows)]
fn find_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        for extension in path_extensions() {
            let candidate = dir.join(format!("{name}{extension}"));
            if is_executable(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

#[cfg(windows)]
fn path_extensions() -> Vec<String> {
    let raw = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_owned());
    raw.split(';')
        .map(str::trim)
        .filter(|extension| extension.starts_with('.'))
        .map(str::to_ascii_lowercase)
        .collect()
}

#[cfg(unix)]
fn find_well_known() -> Option<PathBuf> {
    None
}

/// The installer locations a Windows user is likely to have: the per-user
/// installer, the WinGet package store and Steam.
#[cfg(windows)]
fn find_well_known() -> Option<PathBuf> {
    let mut directories = Vec::new();
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        let local = PathBuf::from(local);
        directories.push(local.join(r"Programs\Godot"));
        let winget = local.join(r"Microsoft\WinGet\Packages");
        if let Ok(entries) = std::fs::read_dir(&winget) {
            for entry in entries.flatten() {
                if entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("GodotEngine.GodotEngine")
                {
                    directories.push(entry.path());
                }
            }
        }
    }
    directories.push(PathBuf::from(
        r"C:\Program Files (x86)\Steam\steamapps\common\Godot Engine",
    ));
    for directory in directories {
        if let Some(found) = newest_godot_in(&directory) {
            return Some(found);
        }
    }
    None
}

/// The highest-sorting `godot*.exe` in a directory, so a store that keeps
/// several versions side by side yields the newest one.
#[cfg(windows)]
fn newest_godot_in(directory: &Path) -> Option<PathBuf> {
    let mut candidates = std::fs::read_dir(directory)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .map(|name| name.to_string_lossy().to_ascii_lowercase())
                .is_some_and(|name| {
                    name.starts_with("godot")
                        && name.ends_with(".exe")
                        && !name.contains("_console")
                })
                && is_executable(path)
        })
        .collect::<Vec<_>>();
    candidates.sort();
    candidates.pop()
}

/// The `*_console.exe` twin of a Windows build is a launcher that runs the
/// engine as a child, so the pid the bridge records would own neither the
/// language server port nor the process identity. The engine itself writes to
/// a redirected stdout, so the plain binary is the one to run.
#[cfg(unix)]
fn prefer_direct(path: PathBuf) -> PathBuf {
    path
}

#[cfg(windows)]
fn prefer_direct(path: PathBuf) -> PathBuf {
    let Some(stem) = path
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
    else {
        return path;
    };
    let Some(base) = stem
        .to_ascii_lowercase()
        .ends_with("_console")
        .then(|| stem[..stem.len() - "_console".len()].to_owned())
    else {
        return path;
    };
    let Some(extension) = path
        .extension()
        .map(|ext| ext.to_string_lossy().into_owned())
    else {
        return path;
    };
    let direct = path.with_file_name(format!("{base}.{extension}"));
    if is_executable(&direct) {
        direct
    } else {
        path
    }
}

pub fn check_version(bin: &Path) -> Result<String, String> {
    let mut child = Command::new(bin)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("cannot run {}: {error}", bin.display()))?;
    let stdout = child.stdout.take().map(drain);
    let stderr = child.stderr.take().map(drain);
    let deadline = Instant::now() + VERSION_TIMEOUT;
    loop {
        if child
            .try_wait()
            .map_err(|error| format!("cannot run {}: {error}", bin.display()))?
            .is_some()
        {
            break;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!(
                "Godot at {} did not answer --version within {} s",
                bin.display(),
                VERSION_TIMEOUT.as_secs()
            ));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&collect(stdout)),
        String::from_utf8_lossy(&collect(stderr))
    )
    .trim()
    .to_string();
    if text.starts_with("4.") {
        Ok(text)
    } else {
        Err(format!("Godot at {} is not 4.x: {text}", bin.display()))
    }
}

/// Read one of the child's pipes on its own thread. Godot on Windows blocks
/// on a pipe nobody is reading, however few bytes it has written, so a
/// version poll that only waited would never see the child exit.
fn drain(stream: impl Read + Send + 'static) -> JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut stream = stream;
        let mut bytes = Vec::new();
        let _ = stream.read_to_end(&mut bytes);
        bytes
    })
}

fn collect(reader: Option<JoinHandle<Vec<u8>>>) -> Vec<u8> {
    reader.map_or_else(Vec::new, |reader| reader.join().unwrap_or_default())
}

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

pub fn validate_godot_path(path: &Path) -> Result<(), String> {
    if !path.is_absolute() {
        return Err(format!(
            "godot_path must be an absolute path to an executable: {}",
            path.display()
        ));
    }
    if !is_executable(path) {
        return Err(format!(
            "godot_path is not an existing executable: {}",
            path.display()
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.is_file()
        && std::fs::metadata(path).is_ok_and(|meta| meta.permissions().mode() & 0o111 != 0)
}

/// Windows has no execute bit; an extension in `PATHEXT` is what makes a file
/// runnable.
#[cfg(windows)]
fn is_executable(path: &Path) -> bool {
    path.is_file()
        && path
            .extension()
            .map(|extension| format!(".{}", extension.to_string_lossy().to_ascii_lowercase()))
            .is_some_and(|extension| path_extensions().contains(&extension))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;
    use std::ffi::OsStr;
    use std::fs;
    use std::path::Path;

    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Write a script that answers `--version` with `body`, in the shape the
    /// platform can run: a `#!/bin/sh` script on Unix, a `.cmd` batch file on
    /// Windows.
    #[cfg(unix)]
    fn write_version_script(dir: &Path, name: &str, output: &str) -> PathBuf {
        write_script(dir, name, &format!("#!/bin/sh\necho {output}\n"))
    }

    #[cfg(windows)]
    fn write_version_script(dir: &Path, name: &str, output: &str) -> PathBuf {
        write_script(dir, name, &format!("@echo off\r\necho {output}\r\n"))
    }

    #[cfg(unix)]
    fn write_script(dir: &Path, name: &str, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        fs::write(&path, body).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[cfg(windows)]
    fn write_script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(format!("{name}.cmd"));
        fs::write(&path, body).unwrap();
        path
    }

    #[cfg(unix)]
    fn no_op_script(dir: &Path, name: &str) -> PathBuf {
        write_script(dir, name, "#!/bin/sh\n")
    }

    #[cfg(windows)]
    fn no_op_script(dir: &Path, name: &str) -> PathBuf {
        write_script(dir, name, "@echo off\r\n")
    }

    const NOISE_LINES: usize = 2000;
    const NOISE: &str = "0123456789012345678901234567890123456789012345678901234567890123";

    /// A script that answers `--version` and then writes far more to stderr
    /// than a pipe buffer holds, so a version check that never reads the
    /// child's output while it waits deadlocks instead of answering.
    #[cfg(unix)]
    fn write_noisy_version_script(dir: &Path, name: &str) -> PathBuf {
        write_script(
            dir,
            name,
            &format!(
                "#!/bin/sh\necho 4.7.2.stable\ni=0\nwhile [ $i -lt {NOISE_LINES} ]; do echo {NOISE} >&2; i=$((i+1)); done\n"
            ),
        )
    }

    #[cfg(windows)]
    fn write_noisy_version_script(dir: &Path, name: &str) -> PathBuf {
        write_script(
            dir,
            name,
            &format!(
                "@echo off\r\necho 4.7.2.stable\r\nfor /l %%i in (1,1,{NOISE_LINES}) do @echo {NOISE} 1>&2\r\n"
            ),
        )
    }

    #[test]
    fn output_larger_than_a_pipe_buffer_still_answers() {
        let dir = crate::temp::TempDir::new().unwrap();
        let bin = write_noisy_version_script(dir.path(), "godot");
        let version = version_of(&bin).unwrap();
        assert!(version.starts_with("4.7.2.stable"), "{}", &version[..64]);
        assert!(
            version.len() > NOISE_LINES * NOISE.len(),
            "{}",
            version.len()
        );
    }

    /// With nothing on `PATH` and no `GODOT`, Unix has nowhere left to look.
    /// Windows still checks the installer locations, which may hold a real
    /// Godot on a developer machine.
    fn assert_no_binary_or_well_known() {
        match resolve_godot(None) {
            Ok(found) => {
                assert!(
                    cfg!(windows) && is_executable(&found),
                    "{}",
                    found.display()
                );
            }
            Err(error) => assert_eq!(error, NO_BINARY),
        }
    }

    fn version_of(bin: &Path) -> Result<String, String> {
        loop {
            match check_version(bin) {
                Err(error) if error.contains("Text file busy") => {
                    std::thread::sleep(Duration::from_millis(20))
                }
                result => return result,
            }
        }
    }

    fn with_environment<T>(path: &OsStr, godot: Option<&OsStr>, test: impl FnOnce() -> T) -> T {
        let old_path = env::var_os("PATH");
        let old_godot = env::var_os("GODOT");
        env::set_var("PATH", path);
        match godot {
            Some(value) => env::set_var("GODOT", value),
            None => env::remove_var("GODOT"),
        }
        let result = test();
        match old_path {
            Some(value) => env::set_var("PATH", value),
            None => env::remove_var("PATH"),
        }
        match old_godot {
            Some(value) => env::set_var("GODOT", value),
            None => env::remove_var("GODOT"),
        }
        result
    }

    #[test]
    fn version_4x_is_accepted() {
        let dir = crate::temp::TempDir::new().unwrap();
        let bin = write_version_script(dir.path(), "godot", "4.7.2.stable");
        let version = version_of(&bin).unwrap();
        assert_eq!(version, "4.7.2.stable");
    }

    #[test]
    fn version_3x_is_rejected() {
        let dir = crate::temp::TempDir::new().unwrap();
        let bin = write_version_script(dir.path(), "godot", "3.5");
        let error = version_of(&bin).unwrap_err();
        assert_eq!(error, format!("Godot at {} is not 4.x: 3.5", bin.display()));
    }

    #[cfg(unix)]
    #[test]
    fn version_timeout_is_rejected() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = crate::temp::TempDir::new().unwrap();
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
        let error = version_of(&bin).unwrap_err();
        assert!(error.contains("did not answer --version"), "{error}");
    }

    #[cfg(windows)]
    #[test]
    fn version_timeout_is_rejected() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = crate::temp::TempDir::new().unwrap();
        let bin = write_script(
            dir.path(),
            "godot",
            "@echo off\r\nping -n 20 127.0.0.1 > nul\r\n",
        );
        let error = version_of(&bin).unwrap_err();
        assert!(error.contains("did not answer --version"), "{error}");
    }

    #[test]
    fn resolve_precedence() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = crate::temp::TempDir::new().unwrap();
        let setting = no_op_script(dir.path(), "setting");
        let env_bin = no_op_script(dir.path(), "env-bin");
        let first = dir.path().join("first");
        let second = dir.path().join("second");
        let empty = dir.path().join("empty");
        for path in [&first, &second, &empty] {
            fs::create_dir_all(path).unwrap();
        }
        let first_godot4 = no_op_script(&first, "godot4");
        no_op_script(&first, "godot");
        no_op_script(&second, "godot");

        let search = env::join_paths([&first, &second]).unwrap();
        with_environment(&search, Some(env_bin.as_os_str()), || {
            assert_eq!(resolve_godot(Some(&setting)).unwrap(), setting);
            assert_eq!(resolve_godot(None).unwrap(), env_bin);
            env::set_var("GODOT", "");
            assert_eq!(resolve_godot(None).unwrap(), first_godot4);
            env::remove_var("GODOT");
            env::set_var("PATH", &empty);
            assert_no_binary_or_well_known();
        });
    }

    #[cfg(windows)]
    #[test]
    fn the_engine_wins_over_its_console_launcher() {
        let dir = crate::temp::TempDir::new().unwrap();
        let engine = dir.path().join("Godot_v4.7.2-stable_win64.exe");
        let console = dir.path().join("Godot_v4.7.2-stable_win64_console.exe");
        fs::write(&console, b"MZ").unwrap();
        assert_eq!(prefer_direct(console.clone()), console);
        fs::write(&engine, b"MZ").unwrap();
        assert_eq!(prefer_direct(console), engine.clone());
        assert_eq!(prefer_direct(engine.clone()), engine);
    }

    #[cfg(windows)]
    #[test]
    fn only_files_with_a_pathext_extension_are_executable() {
        let dir = crate::temp::TempDir::new().unwrap();
        for name in ["godot.exe", "godot.cmd", "godot.bat"] {
            let path = dir.path().join(name);
            fs::write(&path, b"MZ").unwrap();
            assert!(is_executable(&path), "{name}");
        }
        let plain = dir.path().join("godot");
        fs::write(&plain, b"MZ").unwrap();
        assert!(!is_executable(&plain));
        let text = dir.path().join("godot.txt");
        fs::write(&text, b"MZ").unwrap();
        assert!(!is_executable(&text));
        assert!(!is_executable(dir.path()));
    }

    #[cfg(windows)]
    #[test]
    fn a_cmd_shim_on_path_is_found() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = crate::temp::TempDir::new().unwrap();
        let shim = write_script(dir.path(), "godot", "@echo off\r\necho 4.7.2.stable\r\n");
        with_environment(dir.path().as_os_str(), None, || {
            assert_eq!(resolve_godot(None).unwrap(), shim);
        });
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

    #[test]
    fn godot_env_must_be_absolute_executable() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = crate::temp::TempDir::new().unwrap();
        let missing = dir.path().join("missing");
        with_environment(dir.path().as_os_str(), Some(missing.as_os_str()), || {
            assert!(resolve_godot(None).is_err());
            env::set_var("GODOT", "relative/godot");
            assert!(resolve_godot(None).is_err());
            env::remove_var("GODOT");
            assert_no_binary_or_well_known();
        });
    }

    #[test]
    fn extra_args_rejects_script_and_export() {
        for arg in [
            "--script",
            "--script=evil.gd",
            "-s",
            "--main-pack",
            "--main-pack=game.pck",
            "--export-release",
            "--export-debug",
            "--export-pack=out.zip",
        ] {
            let args = [arg.to_string()];
            assert!(validate_extra_args(&args).is_err(), "{arg}");
        }
    }
}
