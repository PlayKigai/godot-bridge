# Dev setup (Linux)

Toolchain:

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
rustup target add wasm32-wasip2
```

Godot 4.x on PATH as `godot` (here `/usr/bin/godot`, 4.7.2). Zed installed as
`zeditor` (1.17.2).

Bridge:

```sh
cargo install --path bridge
godot-bridge --version
```

Extension:

1. In Zed, `zed: extensions`, uninstall `GDScript` (GDQuest). It claims the
   same file suffixes.
2. `zed: install dev extension`, pick `extension/`. Zed compiles the
   extension wasm and the three grammars itself.
3. Open a Godot project, open a `.gd` file. `zed: open language server logs`
   shows the bridge log.

Per-project settings go in `.zed/settings.json`:

```json
{
  "lsp": {
    "godot": {
      "settings": {
        "project_dir": "diequest"
      }
    }
  }
}
```

Copy `docs/sample/tasks.json` and `docs/sample/debug.json` into `.zed/` of
the project, or into `~/.config/zed/`.

Zed reads PATH from your login shell. If `cargo install` put `godot-bridge`
in `~/.cargo/bin` and that dir is not on PATH there, symlink it into
`~/.local/bin` or set `lsp.godot.binary.path`.

Runtime files: `$XDG_RUNTIME_DIR/godot-bridge/` holds lock, state, socket and
Godot log per project.
