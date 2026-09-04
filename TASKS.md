# Tasks

Ordered. Each task names its routing per the global rules. "Verify" lines are
what the behavior-verifier checks after the task. All paths are repo-relative.
An implementer reads SPEC.md first and stays inside the named files unless
the task says otherwise. "Depends" lists tasks that must be merged first.

## Phase 0: scaffold and gates

- [x] T0.1 Workspace scaffold. `Cargo.toml` workspace with `bridge/` and
  `extension/`. `bridge/` binary crate (`godot-bridge`) with clap
  subcommands from SPEC, each returning "not implemented" exit 2.
  `extension/` cdylib crate (`godot-zed-extension`) on
  `zed_extension_api = "0.7"`, `extension.toml` with id `godot`, the three
  pinned grammars, one language server `godot`. Extension assets live under `extension/` (`extension/extension.toml`, `extension/languages/`, `extension/debug_adapter_schemas/`). No debug adapter yet. CI
  workflow: `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test`,
  wasm build. Route: codex. Verify: `cargo build -p godot-bridge`,
  `cargo build --target wasm32-wasip2 -p godot-zed-extension`.
- [x] T0.2 Fixtures. `fixtures/minimal-project/` (project.godot, main.tscn
  with a Node2D root and a Label, main.gd attached and defining `_ready`,
  a second `other.gd` with a type error, `sample.gdshader` with a
  `shader_type`, a uniform, a function and a string), `fixtures/nested/repo/game/project.godot`,
  `fixtures/two-projects/a/project.godot` and `b/project.godot`. Godot 4.7
  format, `config_version=5`, main scene set. Route: flash. Verify:
  `godot --headless --path fixtures/minimal-project --quit` exits 0.
- [x] T0.3 Gate: Zed `$ZED_FILE` in debug.json. Manual with Zed 1.17.2 and a
  stub adapter that logs its config. Record the result in `docs/gates.md`.
  Route: inline (needs GUI). Blocks T2.4 and T2.5.

## Phase 1: LSP core

- [x] T1.1 `bridge/src/root.rs`: worktree root and project dir resolution
  per SPEC "Worktree root" and "Project dir", plus the shared
  path-to-file-URI function. Unit tests on fixtures. Route: codex.
- [x] T1.2 `bridge/src/godot_bin.rs`: binary resolution, `--version` check,
  `extra_args` validation. Route: flash.
- [x] T1.3 `bridge/src/state.rs`: runtime dir, both flocks, atomic state
  JSON, pid+start_ticks identity, Unix socket server and client with the
  NDJSON status protocol, stale-state removal. No process killing here.
  Unit tests: second lock rejected, dead-owner state removed, reused pid
  ignored, status round trip, nullable fields while starting. Route: codex.
- [x] T1.4 `bridge/src/process.rs`: spawn with pre_exec (setpgid, PDEATHSIG,
  parent check), piped output with in-memory tail and rotating log, port
  pick, readiness poll with deadline and child-exit detection, group kill
  (SIGTERM, 5 s, SIGKILL, waitpid), kill-by-recorded-pgid for orphans.
  Route: codex-complex.
- [x] T1.5 `bridge/src/framing.rs`: Content-Length codec for tokio with
  configurable cap, single-header writer. Unit tests: split, coalesced,
  oversized, malformed. Route: codex.
- [x] T1.6 `bridge/src/lsp.rs`: the `lsp` subcommand. Startup steps 1 to 8,
  proxy state (open_docs, versions, pending ids, in-flight cap, cancel),
  shutdown, stale orphan cleanup using T1.3 and T1.4, crash recovery.
  Depends: T1.1 to T1.5. Route: codex-complex. Verify: integration tests
  from SPEC "Integration" for lsp, nested, double owner, crash.
- [x] T1.7 `bridge/src/status.rs`: `status` subcommand. Depends: T1.3.
  Route: flash.
- [x] T1.8 Extension `extension/src/lib.rs`: `language_server_command`,
  `language_server_initialization_options`. Route: codex.
- [x] T1.9 Extension languages: `extension/languages/gdscript`, `extension/languages/gdshader`,
  `extension/languages/godot_resource` config.toml per SPEC table and the listed
  queries, written against each pinned grammar's `src/node-types.json`.
  Also extend `docs/dev-setup.md` with GDQuest removal and dev extension
  install steps if missing. Route: codex-complex.
  Verify: `zed: install dev extension`, no query errors in Zed log,
  fixtures highlight.
- [x] T1.10 Manual: Zed on DieQuest with no Godot running gets completion,
  and killing Godot by hand recovers. Route: inline.

## Phase 2: DAP and tasks

- [x] T2.1 `bridge/src/scene.rs`: scene resolution per SPEC. Unit tests:
  adjacent stem, ext_resource reference, lexicographic tie, none.
  Route: codex.
- [x] T2.2 `bridge/src/dap.rs`: `dap` subcommand per SPEC "DAP proxy".
  Depends: T1.3, T1.5, T1.6, T2.1. Route: codex-complex. Verify:
  integration tests "dap without owner" and "dap with owner" from SPEC.
- [x] T2.3 `bridge/src/run.rs`, `bridge/src/doc.rs`,
  `bridge/src/settings_file.rs` (JSONC read of `.zed/settings.json`):
  `run`, `project-dir`, `doc`. Depends: T1.1, T1.2, T2.1. Route: flash.
- [x] T2.4 Extension DAP: `dap_request_kind`, `get_dap_binary`,
  `dap_config_to_scenario`, `extension/debug_adapter_schemas/godot.json`, add
  `[debug_adapters.godot]` to `extension/extension.toml` and `debuggers = ["godot"]`
  to the GDScript config. Depends: T0.3, T1.8. Route: codex.
- [x] T2.5 Review `docs/sample/.zed/tasks.json` and `debug.json` against
  the T0.3 result; drop the current-file template if the gate failed.
  Remove the `godot: open editor` task from the sample until T4.1.
  Depends: T0.3. Route: flash.
- [x] T2.6 Manual: breakpoint hit in DieQuest from "launch main scene".
  Route: inline.

## Phase 3: project-wide features

- [x] T3.1 `bridge/src/docs_state.rs` and its integration in `lsp.rs`:
  owner state machine, bulk didOpen, notify watcher, debounce, empty
  diagnostics on removal. Depends: T1.6. Route: codex-complex. Verify:
  diagnostics for `fixtures/minimal-project/other.gd` appear without Zed
  opening it.
- [x] T3.2 `bridge/src/symbols.rs` and its integration in `lsp.rs`:
  documentSymbol cache, `workspace/symbol`, capability patch. Depends: T3.1.
  Route: codex. Verify: `workspace/symbol` with query "ready" returns
  `_ready` from the fixture.

## Phase 4: GUI hand-off

- [x] T4.1 `open-editor` subcommand, `handoff` socket handling in `lsp.rs`
  and `state.rs`, gui mode rules, swap back on GUI exit, and add the
  `godot: open editor` task back to `docs/sample/.zed/tasks.json`.
  Depends: T2.3, T3.2. Route: codex-complex. Verify manual: completion survives hand-off
  and swap back.

## Later

- Publish bridge releases and add extension auto-download.
- macOS and Windows process management.
- Contribute upstream or to the Zed extension registry.
