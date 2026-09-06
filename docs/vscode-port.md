# VS Code port

The bridge is editor-agnostic. A VS Code extension is TypeScript glue, the
role `extension/` plays for Zed.

Bridge change: `run`, `open-editor`, `doc` and `project-dir` read Zed's
settings files. Make them accept `GODOT_BRIDGE_SETTINGS`, the env var `dap`
already uses, and skip the Zed files when it is set. About 30 lines.

| piece | VS Code |
|---|---|
| language server | `vscode-languageclient`, command `godot-bridge lsp`, `initializationOptions` from `godot.*` settings |
| debug adapter | `contributes.debuggers` type `godot`, attributes from `debug_adapter_schemas/godot.json`, `DebugAdapterDescriptorFactory` running `godot-bridge dap [--file ${file}]` with `GODOT_BRIDGE_SETTINGS` |
| syntax | TextMate grammar, reuse godot-tools (MIT) |
| indentation | `language-configuration.json`: increase on `:\s*$`, decrease on `else`/`elif`, tabs |
| commands | run project, run current scene, open editor, docs, status: spawn the bridge subcommand with the env var and the workspace folder as cwd |
| settings | `godot.godotPath`, `projectDir`, `extraArgs`, `lspPort`, `dapPort`, `startupTimeoutS`, `projectDiagnostics`, `diagnoseAddons`, mapped 1:1 |

Restart the client on settings change. Document uninstalling godot-tools,
which claims `.gd` and its own debug type. The bridge runs on Linux and
Windows, not macOS, which matters more for VS Code users than for Zed.
