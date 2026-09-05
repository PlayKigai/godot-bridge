# Godot for Zed

GDScript in Zed, backed by the Godot editor's own language server and debugger.
Linux only. Godot 4.x.

## Features

- Syntax highlighting for GDScript, `.tscn`/`.tres` resources and shaders.
- Completion, hover, go to definition, rename, document symbols. Godot's own
  server does the work, so results match the editor.
- Diagnostics for every script in the project, not only open files. Updates
  when files change on disk.
- Workspace symbol search (`cmd-t` / `ctrl-t`) with fuzzy matching across all
  scripts.
- Debugging: breakpoints, stepping, variables, launch the main scene or the
  scene of the current file, or attach.
- Tasks: run the project, run the current scene, open the Godot editor, open
  class docs for the symbol under the cursor.
- Godot runs headless in the background. `godot: open editor` swaps to the
  GUI editor with completion intact. Closing the GUI swaps back to headless.

## Install

1. Godot 4 on your PATH as `godot`, or set `godot_path` below.
2. Build the bridge:

   ```sh
   cargo install --path bridge --locked
   ```

3. In Zed, uninstall the GDQuest `GDScript` extension if present. Then
   `zed: install dev extension` and pick `extension/`.
4. Open a Godot project and a `.gd` file. Godot starts by itself.

Copy `docs/sample/.zed/tasks.json` and `debug.json` into your project's
`.zed/` for the tasks and debug configurations.

## Settings

`lsp.godot.settings` in Zed user settings or the project's `.zed/settings.json`.
Restart the language server after changes.

```json
{ "lsp": { "godot": { "settings": { "project_dir": "game" } } } }
```

| key | default | meaning |
|---|---|---|
| `godot_path` | `godot` on PATH | Godot binary. User settings only. |
| `project_dir` | auto | Directory with `project.godot`, relative to the worktree. Needed when the worktree holds several. User settings only. |
| `lsp_port` | unset | Attach to a Godot editor you started yourself instead of spawning one. |
| `dap_port` | 6006 | Its debug port. Read only with `lsp_port`. |
| `project_diagnostics` | true | Diagnostics for all scripts, not only open ones. |
| `diagnose_addons` | false | Include `addons/` in that. |
| `extra_args` | `[]` | Extra Godot command line flags. User settings only. |
| `startup_timeout_s` | 600 | How long to wait for Godot to come up. |

## Using the GUI editor

- Run the task `godot: open editor`. Zed keeps working while the GUI is open,
  and returns to headless when you close it.
- Opening Godot yourself from a launcher does not swap. Zed keeps its
  headless instance and both run side by side. To always use your own
  editor, set `lsp_port` to its language server port.

## Resources

Godot does the heavy work. The bridge stays under 10 MB and idles at zero
CPU. Typing costs it well under 0.1 percent of a core.

## Troubleshooting

- `zed: open language server logs` shows the bridge log. Set
  `GODOT_BRIDGE_LOG=debug` in Zed's environment for more.
- `godot-bridge status` prints the state of every running bridge.
- Godot's own output is in `$XDG_RUNTIME_DIR/godot-bridge/*.godot.log`.
- Reset everything: quit Zed, then

  ```sh
  pkill -x godot-bridge; pkill -x godot
  rm -rf "${XDG_RUNTIME_DIR:-/tmp/godot-bridge-$(id -u)}/godot-bridge"
  ```

  That directory is the only thing the bridge writes. Symbols and
  diagnostics live in memory and rebuild on restart.
- One Zed window per project. A second window gets an error from Godot's
  one-client limit.
