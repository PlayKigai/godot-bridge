use zed_extension_api as zed;

struct GodotExtension;

impl zed::Extension for GodotExtension {
    fn new() -> Self {
        Self
    }

    fn language_server_command(
        &mut self,
        _language_server_id: &zed::LanguageServerId,
        worktree: &zed::Worktree,
    ) -> zed::Result<zed::Command> {
        let settings = zed::settings::LspSettings::for_worktree("godot", worktree)?;
        let command_settings = settings.binary.as_ref();
        let command = command_settings
            .and_then(|binary| binary.path.clone())
            .or_else(|| worktree.which("godot-bridge"))
            .ok_or_else(|| "Install godot-bridge: cargo install --path bridge".to_string())?;
        let mut args = vec!["lsp".to_string()];
        if let Some(binary_arguments) =
            command_settings.and_then(|binary| binary.arguments.as_ref())
        {
            args.push("--".to_string());
            args.extend(binary_arguments.iter().cloned());
        }

        Ok(zed::Command {
            command,
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
        let settings = zed::settings::LspSettings::for_worktree("godot", worktree)?;
        let binary_settings = settings.binary.as_ref();
        let command = binary_settings
            .and_then(|binary| binary.path.clone())
            .or_else(|| worktree.which("godot-bridge"))
            .ok_or_else(|| "Install godot-bridge: cargo install --path bridge".to_string())?;

        let mut arguments = vec!["dap".to_string()];
        if let Some(file) = config_value.get("file").and_then(|file| file.as_str()) {
            arguments.extend(["--file".to_string(), file.to_string()]);
        }
        if let Some(binary_arguments) = binary_settings.and_then(|binary| binary.arguments.as_ref())
        {
            arguments.push("--".to_string());
            arguments.extend(binary_arguments.iter().cloned());
        }

        let request = self.dap_request_kind(config.adapter.clone(), config_value)?;
        let settings_json = settings
            .settings
            .as_ref()
            .map(zed::serde_json::to_string)
            .transpose()
            .map_err(|error| error.to_string())?
            .unwrap_or_else(|| "{}".to_string());
        let mut envs = vec![("GODOT_BRIDGE_SETTINGS".to_string(), settings_json)];
        envs.extend(worktree.shell_env());

        Ok(zed::DebugAdapterBinary {
            command: Some(command),
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
}

zed::register_extension!(GodotExtension);
