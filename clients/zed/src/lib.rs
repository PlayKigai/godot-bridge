use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use zed_extension_api as zed;

const CARGO_LINE: &str = "Install it: cargo install godot-bridge --locked";

mod sha256;

struct GodotExtension;

/// The bridge trusts GODOT_BRIDGE_SETTINGS, so a direnv-provided one must not leak through.
fn shell_env(worktree: &zed::Worktree) -> Vec<(String, String)> {
    let mut env = worktree.shell_env();
    env.retain(|(key, _)| key != "GODOT_BRIDGE_SETTINGS");
    env
}

/// Wasm `Path::is_absolute` knows only Unix roots.
fn is_absolute(path: &str) -> bool {
    let bytes = path.as_bytes();
    path.starts_with(['/', '\\'])
        || (bytes.len() > 2
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && matches!(bytes[2], b'/' | b'\\'))
}

/// Zed applies project settings only for trusted worktrees; relative paths would resolve against the project.
fn bridge_command(
    worktree: &zed::Worktree,
    id: Option<&zed::LanguageServerId>,
) -> zed::Result<String> {
    let (asset, linux) = platform_asset()?;
    let configured = zed::settings::LspSettings::for_worktree("godot", worktree)?
        .binary
        .and_then(|binary| binary.path);
    let binary_name = match configured {
        Some(path) if is_absolute(&path) => return Ok(path),
        Some(path) if path.contains('/') || path.contains('\\') || path.starts_with('~') => {
            "godot-bridge".to_string()
        }
        Some(path) => path,
        None => "godot-bridge".to_string(),
    };
    if std::fs::metadata(&asset).is_ok_and(|metadata| metadata.is_file()) {
        return absolute(&asset);
    }
    if let Some(path) = worktree.which(&binary_name) {
        return Ok(path);
    }
    download(&asset, linux, id)
}

fn platform_asset() -> zed::Result<(String, bool)> {
    let (os, arch) = zed::current_platform();
    let linux = match os {
        zed::Os::Linux => true,
        zed::Os::Windows => false,
        zed::Os::Mac => {
            return Err(
                "godot-bridge: macOS is not supported. Linux and Windows only.".to_string(),
            );
        }
    };
    let os_name = if linux { "linux" } else { "windows" };
    let triple = match arch {
        zed::Architecture::X8664 => "x86_64",
        zed::Architecture::Aarch64 => "aarch64",
        zed::Architecture::X86 => {
            return Err(format!(
                "godot-bridge: no prebuilt binary for {os_name}/x86. {CARGO_LINE}"
            ));
        }
    };
    let ext = if linux { "" } else { ".exe" };
    let version = env!("CARGO_PKG_VERSION");
    Ok((
        format!("godot-bridge-v{version}-{triple}-{os_name}{ext}"),
        linux,
    ))
}

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn temp_suffix() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    let count = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{nanos}-{count}")
}

fn absolute(asset: &str) -> zed::Result<String> {
    std::env::current_dir()
        .map(|dir| dir.join(asset).to_string_lossy().into_owned())
        .map_err(|error| error.to_string())
}

fn expected_hash(sums: &str, asset: &str) -> zed::Result<String> {
    let mut matches = 0;
    let mut digest = String::new();
    for line in sums.lines() {
        let line = line.trim_end();
        let Some((candidate, rest)) = line.split_at_checked(64) else {
            continue;
        };
        if !candidate.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            continue;
        }
        let Some(name) = rest.strip_prefix("  ").or_else(|| rest.strip_prefix(" *")) else {
            continue;
        };
        if name == asset {
            matches += 1;
            digest = candidate.to_string();
        }
    }
    match matches {
        1 => Ok(digest),
        0 => Err(format!(
            "godot-bridge: {asset} is absent from this release's SHA256SUMS"
        )),
        _ => Err(format!(
            "godot-bridge: SHA256SUMS lists {asset} more than once"
        )),
    }
}

fn failed(id: Option<&zed::LanguageServerId>, message: String) -> String {
    if let Some(id) = id {
        zed::set_language_server_installation_status(
            id,
            &zed::LanguageServerInstallationStatus::Failed(message.clone()),
        );
    }
    format!("{message}. {CARGO_LINE}")
}

fn install(asset: &str, linux: bool, tag: &str, expected: &str, base: &str) -> zed::Result<String> {
    let part = format!("{asset}.{}.part", temp_suffix());
    let url = format!("{base}/{tag}/{asset}");
    zed::download_file(&url, &part, zed::DownloadedFileType::Uncompressed).inspect_err(|_| {
        let _ = std::fs::remove_file(&part);
    })?;
    let bytes = std::fs::read(&part).map_err(|error| {
        let _ = std::fs::remove_file(&part);
        error.to_string()
    })?;
    let actual = sha256::sha256_hex(&bytes);
    if !actual.eq_ignore_ascii_case(expected) {
        let _ = std::fs::remove_file(&part);
        return Err(format!(
            "godot-bridge: SHA-256 mismatch for {asset}: expected {expected}, got {actual}"
        ));
    }
    if std::fs::rename(&part, asset).is_err() {
        let matched = std::fs::read(asset)
            .map(|current| sha256::sha256_hex(&current).eq_ignore_ascii_case(expected))
            .unwrap_or(false);
        let _ = std::fs::remove_file(&part);
        if !matched {
            return Err(format!(
                "godot-bridge: could not replace {asset}: stop the language server and retry"
            ));
        }
    }
    if linux {
        zed::make_file_executable(asset)?;
    }
    absolute(asset)
}

fn download(asset: &str, linux: bool, id: Option<&zed::LanguageServerId>) -> zed::Result<String> {
    const BASE: &str = "https://github.com/PlayKigai/godot-bridge/releases/download";
    let tag = format!("v{}", env!("CARGO_PKG_VERSION"));
    if let Some(id) = id {
        zed::set_language_server_installation_status(
            id,
            &zed::LanguageServerInstallationStatus::CheckingForUpdate,
        );
    }
    let sums_name = format!("SHA256SUMS.{}", temp_suffix());
    let sums_url = format!("{BASE}/{tag}/SHA256SUMS");
    let parsed = (|| -> zed::Result<String> {
        zed::download_file(&sums_url, &sums_name, zed::DownloadedFileType::Uncompressed)?;
        let text = std::fs::read_to_string(&sums_name).map_err(|error| error.to_string())?;
        expected_hash(&text, asset)
    })();
    let _ = std::fs::remove_file(&sums_name);
    let expected = match parsed {
        Ok(expected) => expected,
        Err(message) => return Err(failed(id, message)),
    };
    if let Some(id) = id {
        zed::set_language_server_installation_status(
            id,
            &zed::LanguageServerInstallationStatus::Downloading,
        );
    }
    match install(asset, linux, &tag, &expected, BASE) {
        Ok(path) => {
            if let Some(id) = id {
                zed::set_language_server_installation_status(
                    id,
                    &zed::LanguageServerInstallationStatus::None,
                );
            }
            Ok(path)
        }
        Err(message) => Err(failed(id, message)),
    }
}

impl zed::Extension for GodotExtension {
    fn new() -> Self {
        Self
    }

    fn language_server_command(
        &mut self,
        language_server_id: &zed::LanguageServerId,
        worktree: &zed::Worktree,
    ) -> zed::Result<zed::Command> {
        Ok(zed::Command {
            command: bridge_command(worktree, Some(language_server_id))?,
            args: vec!["lsp".to_string()],
            env: shell_env(worktree),
        })
    }

    fn language_server_initialization_options(
        &mut self,
        _language_server_id: &zed::LanguageServerId,
        worktree: &zed::Worktree,
    ) -> zed::Result<Option<zed::serde_json::Value>> {
        Ok(zed::settings::LspSettings::for_worktree("godot", worktree)?.settings)
    }

    fn dap_request_kind(
        &mut self,
        _adapter_name: String,
        config: zed::serde_json::Value,
    ) -> zed::Result<zed::StartDebuggingRequestArgumentsRequest, String> {
        match config.get("request").and_then(|request| request.as_str()) {
            Some("launch") => Ok(zed::StartDebuggingRequestArgumentsRequest::Launch),
            Some("attach") => Ok(zed::StartDebuggingRequestArgumentsRequest::Attach),
            Some(request) => Err(format!("Unsupported Godot debug request: {request}")),
            None => Err("Godot debug configuration is missing request".to_string()),
        }
    }

    fn get_dap_binary(
        &mut self,
        _adapter_name: String,
        config: zed::DebugTaskDefinition,
        _user_provided_debug_adapter_path: Option<String>,
        worktree: &zed::Worktree,
    ) -> zed::Result<zed::DebugAdapterBinary, String> {
        let config_value: zed::serde_json::Value =
            zed::serde_json::from_str(&config.config).map_err(|error| error.to_string())?;
        let file = config_value
            .get("file")
            .and_then(|file| file.as_str())
            .map(str::to_owned);
        let request = self.dap_request_kind(config.adapter, config_value)?;

        let mut arguments = vec!["dap".to_string()];
        if let Some(file) = file {
            arguments.extend(["--file".to_string(), file]);
        }

        Ok(zed::DebugAdapterBinary {
            command: Some(bridge_command(worktree, None)?),
            arguments,
            envs: shell_env(worktree),
            cwd: Some(worktree.root_path()),
            connection: None,
            request_args: zed::StartDebuggingRequestArguments {
                configuration: config.config,
                request,
            },
        })
    }

    fn dap_config_to_scenario(
        &mut self,
        config: zed::DebugConfig,
    ) -> zed::Result<zed::DebugScenario, String> {
        let adapter_config = match config.request {
            zed::DebugRequest::Launch(launch) => zed::serde_json::json!({
                "request": "launch",
                "scene": if launch.program.ends_with(".tscn") {
                    launch.program
                } else {
                    "main".to_string()
                },
            }),
            zed::DebugRequest::Attach(_) => zed::serde_json::json!({
                "request": "attach",
            }),
        };

        Ok(zed::DebugScenario {
            label: config.label,
            adapter: config.adapter,
            build: None,
            config: zed::serde_json::to_string(&adapter_config)
                .map_err(|error| error.to_string())?,
            tcp_connection: None,
        })
    }
    /// Lets the debug picker offer the shipped run tasks without a `debug.json`.
    fn dap_locator_create_scenario(
        &mut self,
        _locator_name: String,
        build_task: zed::TaskTemplate,
        resolved_label: String,
        debug_adapter_name: String,
    ) -> Option<zed::DebugScenario> {
        if debug_adapter_name != "godot"
            || build_task.command != "godot-bridge"
            || build_task.args.first().map(String::as_str) != Some("run")
        {
            return None;
        }
        let value_after = |flag: &str| {
            build_task
                .args
                .iter()
                .position(|arg| arg == flag)
                .and_then(|index| build_task.args.get(index + 1))
        };
        let mut adapter_config = zed::serde_json::json!({
            "request": "launch",
            "scene": value_after("--scene").map_or("main", String::as_str),
        });
        if let Some(file) = value_after("--file") {
            adapter_config["file"] = zed::serde_json::Value::String(file.clone());
        }
        Some(zed::DebugScenario {
            label: resolved_label,
            adapter: debug_adapter_name,
            build: None,
            config: adapter_config.to_string(),
            tcp_connection: None,
        })
    }
}

zed::register_extension!(GodotExtension);
