# Gates

## T0.3: `$ZED_FILE` substitution in debug.json (Zed 1.17.2)

Passed by source inspection of Zed v1.17.2 (commit c8e44cf), 2026-09-04.

`RunningState::resolve_scenario` (`crates/debugger_ui/src/session/running.rs`)
calls `substitute_variables_in_config`, which walks every string in the
scenario `config` object recursively, nested objects and arrays included, and
applies task variable substitution. The substituted config then reaches the
extension as `DebugTaskDefinition.config`, a compact JSON string
(`crates/extension_host/src/wasm_host/wit/since_v0_8_0.rs`, `TryFrom`).
Unknown `$ZED_*` variables are left unchanged. `$ZED_FILE` resolves only when
a buffer is active. The "scene of current file" template stays. T2.6 confirms
this on a real run.

## Phase 3 measurements (2026-09-04, DieQuest, 1235 scripts, Godot 4.7.2)

| measurement | result |
|---|---|
| headless editor start to both ports bound | 8 s |
| bulk didOpen 400 files, 20 per 50 ms | 1.5 s, 400 publishDiagnostics |
| serial didOpen + documentSymbol | 80 ms per file |
| burst of 100 documentSymbol on open files | 90 ms total |
| 400-request documentSymbol burst after bulk open | Godot segfault once in three runs (dummy renderer, Label3D::_shape) |

Derived constants: bulk open 20 per 50 ms, documentSymbol debounce 300 ms,
in-flight cap 32.
