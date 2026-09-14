# Godot Bridge

GDScript in Zed, VS Code and Neovim, served by the Godot editor's own
language server and debugger. Linux and Windows, Godot 4.x.

## Features

**Language**

- Highlighting for GDScript, `.tscn`/`.tres` resources and shaders.
- Completion, hover, go to definition, rename and symbols from Godot.
- Diagnostics and workspace symbols for every script in the project, not
  only open ones. Hidden directories, `addons/` unless `diagnose_addons`,
  symlinks, invalid UTF-8 and files over 2 MiB are skipped; `exclude` skips
  more.

**Debugging**

- Breakpoints, stepping, variables.
- Launch the main scene, launch the scene of the current file, or attach.

**Commands**

- Run project, run current scene, open the Godot editor, class docs for the
  symbol under the cursor, bridge status.

**Godot lifecycle**

- Godot starts headless when you open a `.gd` file. No window, no setup.
- Open editor swaps to the GUI editor with completion intact and swaps back
  when it closes.

## How it works

One Rust binary, `godot-bridge`, owns Godot and proxies LSP and DAP. Each
editor gets a thin client that spawns it.

| dir | contents |
|---|---|
| `bridge/` | the binary: LSP proxy, DAP proxy, Godot lifecycle |
| `clients/zed/` | Zed extension (wasm) |
| `clients/vscode/` | VS Code extension (TypeScript) |
| `lua/`, `plugin/`, `doc/` | Neovim plugin, at the repo root so plugin managers find it |

## Install

Godot 4 must be installed. Every editor needs the bridge binary first, then
its own client.

### The bridge

Needs a Rust toolchain from [rustup](https://rustup.rs).

```sh
cargo install godot-bridge --locked
godot-bridge --version
```

Prebuilt Linux and Windows binaries are attached to each
[release](https://github.com/PlayKigai/godot-bridge/releases/latest) with a
`SHA256SUMS` file and a build provenance attestation. `SECURITY.md` has the
verification commands.

The binary lands in `~/.cargo/bin` (`%USERPROFILE%\.cargo\bin` on Windows).
Editors start with your login shell's PATH, not your terminal's. If the
second command works in a terminal but the editor cannot find the bridge,
symlink it into `~/.local/bin` or set the bridge path setting of your editor.

Godot is taken from `godot_path` (see Settings), else `$GODOT`, else `godot4`
or `godot` on PATH. On Windows the bridge also looks in
`%LOCALAPPDATA%\Programs\Godot`, the WinGet package store and Steam.

### Zed

1. Disable any other GDScript extension. Two language servers on `.gd`
   conflict.
2. `git clone https://github.com/PlayKigai/godot-bridge`.
3. In Zed run `zed: install dev extension` and pick the `clients/zed`
   folder of the clone. Zed compiles it and needs `cargo` on its PATH.
4. Open a `.gd` file and wait for the first diagnostics.

Bridge path: `lsp.godot.binary.path` in user settings, absolute. Other
settings go under `lsp.godot.settings`:

```json
{ "lsp": { "godot": { "settings": { "project_dir": "game" } } } }
```

Zed applies a project's `.zed/settings.json` only after you trust the
worktree.

Tasks appear in the task picker when a GDScript file is active:
`godot: run project`, `godot: run current scene`, `godot: open editor`,
`godot: docs for symbol`, `godot: bridge status`. The debug picker offers
launch main scene, launch current scene and attach.

### VS Code

1. Disable any other GDScript extension. Two language servers on `.gd`
   and two `godot` debug types conflict.
2. Download the `.vsix` from the
   [latest release](https://github.com/PlayKigai/godot-bridge/releases/latest)
   and install it:
   ```sh
   code --install-extension godot-bridge-*.vsix
   ```
   To build it yourself: `cd clients/vscode && npm ci && npm run package`.
3. Open a `.gd` file.

Bridge path: `godot.bridgePath`. Other settings are `godot.godotPath`,
`godot.projectDir` and the rest of the Settings table in camelCase.
Commands are in the palette under `Godot:`. Press F5 and pick Godot to
debug; run project, run current scene and attach are offered and VS Code
writes the `launch.json`.

macOS is not supported. The extension loads, shows an error and stays idle.

### Neovim

Neovim 0.10 or newer. With lazy.nvim:

```lua
{ "PlayKigai/godot-bridge", ft = "gdscript", opts = {} }
```

Open a `.gd` file. Commands: `:GodotRun`, `:GodotRunScene`,
`:GodotEditor`, `:GodotDoc`, `:GodotStatus`, `:GodotRestart`.

Bridge path and other settings:

```lua
opts = {
  bridge_path = vim.fn.expand("~/.cargo/bin/godot-bridge"),
  settings = { project_dir = "game" },
}
```

Debugging needs nvim-dap. After setup call
`require("godot-bridge").dap()`; `:DapContinue` then offers run project,
run current scene and attach. Highlighting comes from nvim-treesitter's
`gdscript` parser. `:help godot-bridge` has the rest.

## Settings

Every client exposes these keys in its own casing, plus a bridge path key
(`lsp.godot.binary.path`, `godot.bridgePath`, `bridge_path`). Keys marked
"user only" are read from user settings, never from a project. VS Code and
Neovim also take the bridge path from user settings only. Zed applies project
settings only for trusted worktrees.

| key | default | meaning |
|---|---|---|
| `godot_path` | `$GODOT`, else `godot4` or `godot` on PATH | Godot binary. User only. |
| `project_dir` | auto | Directory with `project.godot`, relative to the workspace root. User only. |
| `lsp_port` | unset | Attach to an editor you started yourself instead of spawning one. User only. |
| `dap_port` | 6006 | Debug port of the editor named by `lsp_port`. Ignored otherwise. User only. |
| `project_diagnostics` | true | Diagnostics for all scripts, not only open ones. |
| `diagnose_addons` | false | Include `addons/`. |
| `exclude` | `[]` | Glob patterns kept out of the project-wide diagnostics and symbol scan and out of scene lookup. A bare name matches at any depth, a `/` or `./` before the name anchors the pattern to the project root except a leading `**/`, a trailing `/` matches directories only. `*`, `?` and `**`, no negation. |
| `extra_args` | `[]` | Extra Godot flags. User only. |
| `startup_timeout_s` | 600 | How long to wait for Godot. |

Zed reads these from its settings files. VS Code and Neovim pass them to
the bridge as one JSON object in `GODOT_BRIDGE_SETTINGS`; when that is set,
no settings file is read.

## Troubleshooting

- The language server log in your editor is the bridge log.
  `GODOT_BRIDGE_LOG=debug` in the editor's environment for more.
- `godot-bridge status` prints every running bridge.
- Godot's output: `$XDG_RUNTIME_DIR/godot-bridge/*.godot.log`, on Windows
  `%LOCALAPPDATA%\godot-bridge\*.godot.log` (`%TEMP%\godot-bridge` when
  `%LOCALAPPDATA%` is unset).
- Debugging needs the language server running for that project unless
  `lsp_port` is set. Open a `.gd` file of the project before starting a debug
  session.
- One editor window per project. Godot serves one client.
- Reset: quit the editor, `pkill -x godot-bridge; pkill -x godot`, delete
  `$XDG_RUNTIME_DIR/godot-bridge` and `/tmp/godot-bridge-$UID`, the only
  places the bridge writes. On Windows, `taskkill /IM godot-bridge.exe /F`,
  end any leftover Godot process, then delete `%LOCALAPPDATA%\godot-bridge`
  (`%TEMP%\godot-bridge` when `%LOCALAPPDATA%` is unset).
- Windows: the Godot editor keeps running after the editor exits only if it
  broke out of the bridge's job object. A log line says so when the policy
  forbids it.

## Development

```sh
rustup target add wasm32-wasip2
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test -p godot-bridge            # integration tests need Godot installed
python3 scripts/check_build_scripts.py   # before anything compiles
cargo deny check && cargo vet
```

The bridge depends on `libc` on Unix and `windows-sys` on Windows, the Zed
extension on `zed_extension_api` only. `deny.toml`, `supply-chain/` and
`scripts/build_scripts.allow` gate new dependencies. Platform code lives in
`bridge/src/sys/`, where `unix/` and `windows/` export the same names; the
rest is shared. After editing queries or `clients/zed/src/lib.rs`, delete
`~/.local/share/zed/extensions/index.json` (Windows:
`%LOCALAPPDATA%\Zed\extensions\index.json`) and restart Zed.

The bridge and every client share one version number and are released
together.

Docs: `docs/spec.md` (design and protocol), `docs/release.md` (cutting a
release and recovering from a failed one), `docs/mac-port.md`.
