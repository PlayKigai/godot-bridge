use zed_extension_api as zed;

struct GodotExtension;

struct BridgeConfig {
    settings: zed::settings::LspSettings,
    command: String,
    binary_arguments: Vec<String>,
}

fn bridge_config(worktree: &zed::Worktree) -> zed::Result<BridgeConfig> {
    let settings = zed::settings::LspSettings::for_worktree("godot", worktree)?;
    let binary = settings.binary.as_ref();
    let command = binary
        .and_then(|binary| binary.path.clone())
        .or_else(|| worktree.which("godot-bridge"))
        .ok_or_else(|| "Install godot-bridge: cargo install --path bridge".to_string())?;
    let binary_arguments = binary
        .and_then(|binary| binary.arguments.clone())
        .unwrap_or_default();

    Ok(BridgeConfig {
        settings,
        command,
        binary_arguments,
    })
}

impl zed::Extension for GodotExtension {
    fn new() -> Self {
        Self
    }

    fn language_server_command(
        &mut self,
        _language_server_id: &zed::LanguageServerId,
        worktree: &zed::Worktree,
    ) -> zed::Result<zed::Command> {
        let config = bridge_config(worktree)?;
        let mut args = vec!["lsp".to_string()];
        if !config.binary_arguments.is_empty() {
            args.push("--".to_string());
            args.extend(config.binary_arguments);
        }

        Ok(zed::Command {
            command: config.command,
            args,
            env: worktree.shell_env(),
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
        let bridge = bridge_config(worktree)?;

        let mut arguments = vec!["dap".to_string()];
        if let Some(file) = file {
            arguments.extend(["--file".to_string(), file]);
        }
        if !bridge.binary_arguments.is_empty() {
            arguments.push("--".to_string());
            arguments.extend(bridge.binary_arguments);
        }

        let settings_json = bridge
            .settings
            .settings
            .unwrap_or_else(|| zed::serde_json::json!({}));
        let settings_json =
            zed::serde_json::to_string(&settings_json).map_err(|error| error.to_string())?;
        let mut envs = worktree.shell_env();
        envs.retain(|(key, _)| key != "GODOT_BRIDGE_SETTINGS");
        envs.push(("GODOT_BRIDGE_SETTINGS".to_string(), settings_json));

        Ok(zed::DebugAdapterBinary {
            command: Some(bridge.command),
            arguments,
            envs,
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
    /// Turns a `godot-bridge run` task into a launch scenario, so the debug
    /// picker offers the shipped run tasks without a `debug.json`.
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
