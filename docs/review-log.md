# Review log

## Round 1 (codex-thinker, 2026-09-04)

Raw output: `review-1-raw.txt`. 25 findings.

Accepted: didOpen-based root detection deadlocks (root now from rootUri);
no process API in Zed extensions (bridge gained a `dap` stdio proxy
subcommand); no ownership protocol (flock owner, pid start ticks, second
window rejected); port and state races (lock held across spawn, atomic
writes); no startup timeout (600 s default); settings via
initializationOptions only; `project` in DAP launch must be the resolved
dir (proxy rewrites it); dap_config_to_scenario is not a template list
(samples ship in docs); document version ownership; replay with internal
ids; shutdown semantics; PGID and waitpid details; auto-download dropped
from v1; root canonicalization; runtime dir fallback and log rotation;
tasks use bridge subcommands with args arrays.

Rejected, owner decision: dropping project-wide diagnostics, launch current
scene, and GUI hand-off. Kept, moved to phases 3 and 4. Current-scene
resolution now searches scenes referencing the script instead of assuming
an adjacent stem.

## Round 2 (codex-thinker, 2026-09-04)

Raw output: `review-2-raw.txt`. 12 findings, all accepted.

- DAP no longer takes temporary ownership. It needs a live `lsp` owner and
  finds it through the owner socket. Out of scope: debugging without a `.gd`
  open in Zed.
- Owner socket is the readiness authority with a defined NDJSON protocol.
  State file is informational only.
- No adoption of orphan editors. Stale orphans are killed by recorded PGID.
- DAP settings travel in `GODOT_BRIDGE_SETTINGS` from
  `LspSettings::for_worktree`, so user and project settings merge.
- `get_dap_binary` contract fully specified including `request_args`.
- Spawn loop retries on child exit up to 3 attempts, no log-text parsing.
  One retained LSP connection. DAP readiness probed at most once.
- Worktree root derivation defined before project dir resolution.
- `extra_args` rejects bridge-owned flags.
- Fatal behavior for Godot EOF and malformed frames in phases 1 and 2.
- Godot output piped through the bridge for rotation and error tails.
- One DAP session per project via a second lock.

Verdict on phase 1 readiness: was "no" with seven gaps, all closed in v3.

## Round 3 (codex-thinker, 2026-09-04)

Raw output: `review-3-raw.txt`. 18 findings, all accepted. Verdict was "no"
for all four phases.

Changes in v4: initializationOptions type rule; exact worktree URI
selection; nullable status fields while starting; one monotonic deadline
and 3-attempt budget; grammars pinned by URL and commit, per-language
config and query capture list; debug adapter registration moved to phase 2;
T1.3/T1.4/T1.6 responsibilities split; DAP framing and failure response
shape; deterministic scene resolution and `current` rules; launch argument
rewrite rules and schema `additionalProperties: false`; `run` and
`project-dir` semantics; sample paths `docs/sample/.zed/`; recovery queue
rules and cancel forwarding; diagnostics lifecycle details; deterministic
symbol ranking with bridge-internal string ids; phase 3 gate recorded in
`docs/gates.md`; handoff responds at once then works asynchronously; GUI
mode state machine.

Also from measurements this round: Godot's single-header 4 MiB frame limit,
the observed headless segfault, and crash recovery moved to phase 1 with a
32-request in-flight cap.

## Round 4 (codex-thinker, 2026-09-04)

Raw output: `review-4-raw.txt`. 10 findings, all accepted. Phase 0 ready,
others not. Changes in v5: four explicit frame caps; crash before the first
initialize response consumes startup attempts instead of recovery; one
tagged FIFO for recovery queueing; DAP lock taken in every mode; DAP
readiness polling honors timeout 0; bridge-owned DAP seq for every message
with request_seq mapping; watcher didOpen for created files; symbol
responses consumed only for a live URI at the same version; empty-query
order defined; detached GUI state has null owner fields and a reconnect
rule in phase 4, phase 1 never kills a gui-mode process.

## Round 5 (codex-thinker, 2026-09-04)

Raw output: `review-5-raw.txt`. 8 findings, all accepted. Changes in v6:
fixtures define `_ready` and a shader sample; invalid `project_dir` errors
at once; `run --scene current` follows the DAP rule; all request ids to
Godot are bridge-owned with a map back; watcher behavior during recovery;
exact Info message when project diagnostics are off; owner fields nullable
in the schema; `open-editor` without an owner takes the lock and records
gui state with ports.

## Round 6 (codex-thinker, 2026-09-04)

Raw output: `review-6-raw.txt`. The v6 edit had not been written when this
round ran, so it re-reported six round-5 items against v5. No new findings.
v6 now applied; round 7 reviews it.

## Round 7 (codex-thinker, 2026-09-04)

Raw output: `review-7-raw.txt`. 5 findings, all accepted. Changes in v7:
extension asset paths under `extension/` in TASKS; attach mode never
resolves a Godot binary, binary resolved right before spawn;
`dap_config_to_scenario` handles Launch and Attach explicitly; `open_docs`
keyed by canonical path with one URI function; GUI state written as
`starting` right after spawn, a live GUI is never replaced because its
ports are not up yet, GUI spawn failure kills and reaps.

## Round 8 (codex-thinker, 2026-09-04)

Raw output: `review-8-raw.txt`. Phases 0 and 1 ready. 5 findings in phases
2 to 4, all accepted. Changes in v8: DAP reads stdin concurrently during
startup with one ordered buffer; the open-editor task ships with phase 4;
incoming-URI to key mapping survives file deletion; project-wide traffic
starts only after Zed's `initialized`; detached GUI reconnect claims
ownership only after ports are ready.

## Round 9 (codex-thinker, 2026-09-04)

Raw output: `review-9-raw.txt`. Phases 0 and 1 ready. 4 findings, all
accepted. Changes in v9: CLI subcommands merge user and project Zed
settings; watcher removals during recovery delete the entry and clear
diagnostics; deterministic alignment choice for subsequence scoring; kill
path distinguishes own child (`waitpid`) from adopted GUI (wait for pid).

## Round 10 (codex-thinker, 2026-09-04)

Raw output: `review-10-raw.txt`. Phases 0 and 4 ready. 3 findings, all
accepted. Changes in v10: recovery sends `initialized` only if Zed's had
been forwarded before the crash; `doc` uses cwd as worktree root; Zed
`didClose` clears the symbol cache before reopening as Bridge-owned.

## Round 11 (codex-thinker, 2026-09-04)

Raw output: `review-11-raw.txt`. Phases 0 and 2 ready. 3 findings, all
accepted. Changes in v11: recovery FIFO carries responses and drops those
for stale server request ids; watcher paths get a path-to-key mapping so
removals resolve without canonicalizing; hand-off holds the DAP lock for
the whole swap.

## Round 12 (codex-thinker, 2026-09-04)

Raw output: `review-12-raw.txt`. All phases ready. Verdict: READY.
