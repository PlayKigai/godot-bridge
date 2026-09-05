# Godot for Zed

GDScript in Zed, backed by the Godot editor's own language server and
debugger. Linux only. Godot 4.x.

- Highlighting for GDScript, `.tscn`/`.tres` resources and shaders.
- Completion, hover, go to definition, rename, symbols from Godot's server.
- Diagnostics and workspace symbols across eligible scripts, skipping hidden
  paths, addons, symlinks, invalid UTF-8, and files over 2 MiB.
- Debugging: breakpoints, stepping, variables, launch or attach.
- Tasks: run project, run current scene, open the Godot editor, class docs.
- Godot runs headless. `godot: open editor` swaps to the GUI editor with
  completion intact and swaps back when it closes.

## Install

1. Godot 4 on PATH as `godot`, or set `godot_path`.
2. `cargo install --path bridge --locked`
3. In Zed, uninstall the GDQuest `GDScript` extension, then
   `zed: install dev extension` and pick `extension/`.
4. Open a `.gd` file. Godot starts by itself.

Copy `docs/sample/.zed/tasks.json` and `debug.json` into your project's
`.zed/` for tasks and debug configurations.

Zed reads PATH from your login shell. If `~/.cargo/bin` is not on it,
symlink `godot-bridge` into `~/.local/bin` or set `lsp.godot.binary.path`.

## Settings

`lsp.godot.settings` in user settings or `.zed/settings.json`. Restart the
language server after changes.

```json
{ "lsp": { "godot": { "settings": { "project_dir": "game" } } } }
```

| key | default | meaning |
|---|---|---|
| `godot_path` | `godot` on PATH | Godot binary. User settings only. |
| `project_dir` | auto | Directory with `project.godot`, relative to the worktree. User settings only. |
| `lsp_port` | unset | Attach to an editor you started yourself instead of spawning one. |
| `dap_port` | 6006 | Its debug port. Read only with `lsp_port`. |
| `project_diagnostics` | true | Diagnostics for all scripts, not only open ones. |
| `diagnose_addons` | false | Include `addons/`. |
| `extra_args` | `[]` | Extra Godot flags. User settings only. |
| `startup_timeout_s` | 600 | How long to wait for Godot. |

## Troubleshooting

- `zed: open language server logs` shows the bridge log.
  `GODOT_BRIDGE_LOG=debug` in Zed's environment for more.
- `godot-bridge status` prints every running bridge.
- Godot's output: `$XDG_RUNTIME_DIR/godot-bridge/*.godot.log`.
- Reset: quit Zed, `pkill -x godot-bridge; pkill -x godot`, delete that
  directory. It is the only thing the bridge writes.
- One Zed window per project. Godot serves one client.

## Development

```sh
rustup target add wasm32-wasip2
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test -p godot-bridge            # integration tests need /usr/bin/godot
cargo deny check && cargo audit && cargo vet
python3 scripts/check_build_scripts.py
```

The bridge depends on `libc` only, the extension on `zed_extension_api`
only. `deny.toml`, `supply-chain/` and `scripts/build_scripts.allow` gate
new dependencies. After editing queries or `extension/src/lib.rs`, delete
`~/.local/share/zed/extensions/index.json` and restart Zed.

Docs: `docs/spec.md` (design and protocol), `docs/mac-port.md`,
`docs/windows-port.md`, `docs/vscode-port.md`.
