# Godot Bridge

GDScript in Zed, VS Code and Neovim, backed by the Godot editor's own
language server and debugger. Linux and Windows. Godot 4.x.

- Highlighting for GDScript, `.tscn`/`.tres` resources and shaders.
- Completion, hover, go to definition, rename, symbols from Godot's server.
- Diagnostics and workspace symbols across eligible scripts, skipping hidden
  paths, addons, symlinks, invalid UTF-8, and files over 2 MiB.
- Debugging: breakpoints, stepping, variables, launch or attach.
- Commands: run project, run current scene, open the Godot editor, class docs.
- Godot runs headless. Opening the editor swaps to the GUI editor with
  completion intact and swaps back when it closes.

One Rust binary, `godot-bridge`, does all of this. Each editor gets a thin
client that spawns it:

| dir | client |
|---|---|
| `bridge/` | the binary: LSP proxy, DAP proxy, Godot lifecycle |
| `clients/zed/` | Zed extension (wasm) |
| `clients/vscode/` | VS Code extension (TypeScript) |
| `lua/`, `plugin/`, `doc/` | Neovim plugin, at the repo root so plugin managers find it |

## Install

Every editor needs the bridge binary first, then its own client. Linux and
Windows. Godot 4 must be installed.

### 1. Install the bridge

Needs a Rust toolchain from [rustup](https://rustup.rs).

```sh
cargo install godot-bridge --locked
godot-bridge --version
```

Prebuilt Linux and Windows binaries are attached to each
[release](https://github.com/PlayKigai/godot-bridge/releases/latest) with a
`SHA256SUMS` file and a build provenance attestation; `SECURITY.md` has the
verification commands.

This puts `godot-bridge` in `~/.cargo/bin` (`%USERPROFILE%\.cargo\bin` on
Windows). Editors start with your login shell's PATH, not your terminal's,
so if the second command works in a terminal but the editor cannot find the
bridge, symlink it into `~/.local/bin`. VS Code and Neovim can also be
pointed at it with `godot.bridgePath` or `bridge_path` in user settings.

Godot: `godot` on PATH, or set `godot_path` (`lsp.godot.settings.godot_path`
in Zed, `godot.godotPath` in VS Code, `settings.godot_path` in Neovim). On
Windows the bridge also finds Godot in `%LOCALAPPDATA%\Programs\Godot`, the
WinGet package store and Steam.

### 2a. Zed

1. Disable any other GDScript extension. Two language servers on `.gd`
   conflict.
2. `git clone https://github.com/PlayKigai/godot-bridge`.
3. In Zed run `zed: install dev extension` and pick the `clients/zed`
   folder of the clone. Zed compiles it, which needs `cargo` on Zed's PATH.
4. Open a `.gd` file. Godot starts by itself. Wait for the first
   diagnostics.

Bridge path if Zed cannot find it: `lsp.godot.binary.path` in your user
settings, absolute. Zed applies a project's `.zed/settings.json` only after
you trust the worktree. Other settings go under `lsp.godot.settings`, for
example:

```json
{ "lsp": { "godot": { "settings": { "project_dir": "game" } } } }
```

Tasks appear in the task picker when a GDScript file is active:
`godot: run project`, `godot: run current scene`, `godot: open editor`,
`godot: docs for symbol`, `godot: bridge status`. The debug picker offers
launch main scene, launch current scene, and attach.

### 2b. VS Code

1. Disable any other GDScript extension. Two language servers on `.gd`
   and two `godot` debug types conflict.
2. Download the `.vsix` from the
   [latest release](https://github.com/PlayKigai/godot-bridge/releases/latest)
   and install it:
   ```sh
   code --install-extension godot-bridge-*.vsix
   ```
   The extension is not on the Marketplace or Open VSX. To build it
   yourself: `cd clients/vscode && npm ci && npm run package`.
3. Open a `.gd` file. Godot starts by itself.

Bridge path if VS Code cannot find it: `godot.bridgePath`. Other settings
are `godot.godotPath`, `godot.projectDir`, and the Settings table in
camelCase. Commands are in the palette under `Godot:`. Press F5 and pick
Godot for debugging; run project, run current scene and attach are offered
and VS Code writes the `launch.json` for you.

### 2c. Neovim

Neovim 0.10 or newer. With lazy.nvim:

```lua
{ "PlayKigai/godot-bridge", ft = "gdscript", opts = {} }
```

Open a `.gd` file. Godot starts by itself. Commands: `:GodotRun`,
`:GodotRunScene`, `:GodotEditor`, `:GodotDoc`, `:GodotStatus`,
`:GodotRestart`.

Bridge path if Neovim cannot find it, and other settings:

```lua
opts = {
  bridge_path = vim.fn.expand("~/.cargo/bin/godot-bridge"),
  settings = { project_dir = "game" },
}
```

Debugging needs nvim-dap. After setup call
`require("godot-bridge").dap()`, then `:DapContinue` offers run project,
run current scene and attach. Syntax highlighting comes from
nvim-treesitter's `gdscript` parser. `:help godot-bridge` has the rest.

## Settings

Every client exposes these keys in its own casing, plus a bridge path key
(`lsp.godot.binary.path`, `godot.bridgePath`, `bridge_path`). VS Code and
Neovim read it from user settings only; Zed gates project settings behind
worktree trust.

| key | default | meaning |
|---|---|---|
| `godot_path` | `godot` on PATH | Godot binary. User settings only. |
| `project_dir` | auto | Directory with `project.godot`, relative to the workspace root. User settings only. |
| `lsp_port` | unset | Attach to an editor you started yourself instead of spawning one. User settings only. |
| `dap_port` | 6006 | Debug port of the editor named by `lsp_port`. Ignored otherwise. User settings only. |
| `project_diagnostics` | true | Diagnostics for all scripts, not only open ones. |
| `diagnose_addons` | false | Include `addons/`. |
| `extra_args` | `[]` | Extra Godot flags. User settings only. |
| `startup_timeout_s` | 600 | How long to wait for Godot. |

The Zed extension reads these from Zed's settings files. Other clients pass them
to the bridge as one JSON object in `GODOT_BRIDGE_SETTINGS`; when that is
set, no settings file is read.

## Troubleshooting

- The language server log in your editor shows the bridge log.
  `GODOT_BRIDGE_LOG=debug` in the editor's environment for more.
- `godot-bridge status` prints every running bridge.
- Godot's output: `$XDG_RUNTIME_DIR/godot-bridge/*.godot.log`, on Windows
  `%LOCALAPPDATA%\godot-bridge\*.godot.log`.
- Reset: quit the editor, `pkill -x godot-bridge; pkill -x godot`, delete
  that directory. It is the only thing the bridge writes. On Windows,
  `taskkill /IM godot-bridge.exe /F`, end any leftover Godot process, then
  delete `%LOCALAPPDATA%\godot-bridge`.
- Windows: the Godot editor keeps running after the editor exits only if it
  broke out of the bridge's job object. A log line says so when the policy
  forbids it.
- One editor window per project. Godot serves one client.

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
rest of the bridge is shared. After editing queries or `clients/zed/src/lib.rs`,
delete `~/.local/share/zed/extensions/index.json`, on Windows
`%LOCALAPPDATA%\Zed\extensions\index.json`, and restart Zed.

The bridge and every client share one version number and are released
together.

Docs: `docs/spec.md` (design and protocol), `docs/mac-port.md`.
