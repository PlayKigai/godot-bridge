# Godot Bridge for VS Code

GDScript in VS Code, backed by the Godot editor's own language server and
debugger. Linux and Windows. Godot 4.x.

The feature list is in the repository README.

## Install

1. Godot 4 on PATH as `godot`, or set `godot.godotPath`.
2. `cargo install --git https://github.com/PlayKigai/godot-bridge godot-bridge --locked`,
   and make sure `godot-bridge` is on PATH (or set `godot.bridgePath`).
3. Install this extension (`npm run package`, then install the `.vsix`).
4. Disable any other GDScript extension. Two language servers on `.gd`
   and two `godot` debug types conflict.
5. Open a `.gd` file. Godot starts by itself.

On macOS the bridge is not supported; the extension shows an error and stays
idle. One VS Code window per project. Godot serves one client.

## Settings

`godot.*` in user or workspace settings; the bridge, Godot, project and flag
keys are read from user settings only. Only keys you set are sent to the
bridge. Only folders whose effective settings changed restart.

| key | default | meaning |
|---|---|---|
| `godot.bridgePath` | `godot-bridge` on PATH | Bridge binary. User settings only. |
| `godot.godotPath` | `godot` on PATH | Godot binary. User settings only. |
| `godot.projectDir` | auto | Directory with `project.godot`, relative to the workspace root. User settings only. |
| `godot.lspPort` | unset | Attach to an editor you started yourself instead of spawning one. |
| `godot.dapPort` | 6006 | Debug port of the editor named by `lspPort`. Ignored otherwise. |
| `godot.projectDiagnostics` | true | Diagnostics for all scripts, not only open ones. |
| `godot.diagnoseAddons` | false | Include `addons/`. |
| `godot.extraArgs` | `[]` | Extra Godot flags. User settings only. |
| `godot.startupTimeoutS` | 600 | How long to wait for Godot. |

## Commands

All in the Command Palette under "Godot":

| command | meaning |
|---|---|
| Run Project | `godot-bridge run` for the project of the active `.gd`/`.tscn` file |
| Run Current Scene | `godot-bridge run --scene current` for the active `.gd`/`.tscn` file |
| Open Editor | swap headless Godot for the GUI editor |
| Docs for Symbol | open the class reference for the symbol under the cursor |
| Bridge Status | show running bridges in the Godot Bridge output channel |
| Restart Language Server | restart the bridge language client |

Run, editor and docs output show in a terminal. Debugging uses the `godot`
debug type: press F5 and pick Godot for run project, run current scene, and
attach; VS Code writes the `launch.json` for you.

## Troubleshooting

- `Godot: Bridge Status` in the palette, or `godot-bridge status` in a
  terminal, prints every running bridge.
- The Godot Bridge output channel shows the language server log.
- Godot's output: `$XDG_RUNTIME_DIR/godot-bridge/*.godot.log`, on Windows
  `%LOCALAPPDATA%\godot-bridge\*.godot.log`.

## Development

```sh
npm ci
npm run build
npm run package
```
