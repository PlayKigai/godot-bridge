# zed-godot: spec

Status: v11, READY. Twelve codex-thinker review rounds (`docs/review-log.md`).

## Goal

Full GDScript language support in Zed with the Godot editor closed.
Zed opens a `.gd` file, a headless Godot editor starts for that project, Zed
gets completion, hover, go-to-definition, references, rename, project symbol
search, diagnostics, and debugging. Nothing to start by hand.

Reference behavior: `~/dotfiles/shared/.config/nvim/lua/plugins/godot.lua`.

## Verified facts (Godot 4.7.2, Zed 1.17.2, 2026-09-04)

- Zed extensions are WASM. `language_server_command` returns one command that
  Zed spawns on stdio. Host functions: `which`, `read_text_file`,
  `shell_env`, `download_file`, `make_file_executable`. No general "run a
  process" function. `get_dap_binary` may return a command or a TCP
  host/port.
- Godot flags: `--editor --headless --path <dir> --lsp-port <p> --dap-port <q>`.
  Headless editor on DieQuest (1235 scripts) bound both ports in 8 s.
- Godot LSP serves only the most recently connected client
  (`latest_client_id`). Full text sync. On `initialize` it sends
  `gdscript_client/changeWorkspace {path}` then `gdscript/capabilities`.
- Godot's LSP frame reader accepts exactly one header, `Content-Length: N`,
  and a frame cap of 4 MiB (`LSP_MAX_BUFFER_SIZE`). No `Content-Type`.
- Godot LSP registers no `workspace/*` methods. `workspace/symbol` returns
  `-32601 Method not found`.
- Godot publishes diagnostics only for documents a client opened.
- Bulk `didOpen` of 400 scripts, 20 per 50 ms: 1.5 s, 400 diagnostics.
  Serial `didOpen` + `documentSymbol`: 80 ms per file. Burst of 100
  `documentSymbol` on open files: 90 ms total.
- Godot segfaulted once in three runs (dummy renderer, `Label3D::_shape`,
  during a 400-request burst after bulk open). Not reproducible on demand.
  The headless editor can crash under LSP load. Crash recovery is phase 1.
- DAP `launch` from a headless editor spawns the game with a real window.
  The editor passes `--remote-debug` and `--editor-pid` to the game and
  forwards game stdout as DAP `output` events. `scene` accepts `main`,
  `current` (editor's open scene, meaningless headless), or a path. `project`
  is validated against the editor's project. A client that connects to the
  DAP port and closes without sending anything is tolerated.

## Architecture

Zed cannot spawn Godot, poll a port, or kill a child. One native binary,
`godot-bridge`, does all of it. Zed spawns it twice per project: once as the
LSP (`godot-bridge lsp`) and once per debug session as the DAP
(`godot-bridge dap`). Both proxy stdio to the TCP ports of one headless Godot
editor. The LSP bridge owns the editor process.

```
Zed ──stdio──► godot-bridge lsp ──tcp──► Godot editor (headless)
Zed ──stdio──► godot-bridge dap ──tcp──►     ": --lsp-port, --dap-port
                     │ owns, kills, respawns ─┘
```

Repo layout, Cargo workspace:

```
zed-godot/
  SPEC.md
  TASKS.md              ordered work items for orchestration
  bridge/               godot-bridge (Rust, tokio, clap, serde_json, notify, blake3)
  extension/            Zed extension `godot` (Rust -> wasm32-wasip2, zed_extension_api 0.7)
  fixtures/             minimal-project/, nested/repo/game/, two-projects/a/, two-projects/b/
  docs/                 review-log.md, dev-setup.md, gates.md, sample/.zed/{tasks,debug}.json
```

## Phases

Ordered. Each phase ships alone and is testable alone.

| phase | content |
|---|---|
| 1 | Bridge: root, lock, spawn, LSP proxy, lifetime, crash recovery. Extension: grammars, queries, `language_server_command`. |
| 2 | DAP proxy, extension DAP methods, `run`, `project-dir`, `doc`, sample configs. |
| 3 | Project-wide diagnostics, workspace symbols. |
| 4 | GUI hand-off and swap back. |

## Bridge: `godot-bridge`

### Subcommands

| command | purpose |
|---|---|
| `lsp` | Zed's LSP entry. Owns the editor. |
| `dap [--file <path>]` | Zed's DAP entry. Stdio DAP proxy to the editor's DAP port. Requires a running `lsp` owner. |
| `project-dir --file <path>` | Prints the canonical project dir plus newline. Exit 1 with message on failure. |
| `run --file <path> [--scene <value>]` | Runs `<godot> [extra_args] --path <project> [scene]`. `--scene current` follows the DAP `current` rule: a `.tscn` file is used directly as `res://` path, any other file goes through scene resolution, no match exits 1 with "No scene uses <file>". Inherits stdio, waits, returns the child's exit status. |
| `open-editor --file <path>` | Phase 4 hand-off. |
| `status` | Prints one JSON status object per project with a responding socket. Removes stale state files it finds. |
| `doc <symbol>` | Opens the class reference page in the browser. |

Any subcommand accepts `--` followed by `binary.arguments` from Zed; those
are ignored today and reserved.

### Settings

Zed settings path `lsp.godot.settings` in user settings or
`.zed/settings.json`. User settings control the Godot executable, project path,
and extra arguments. Those three keys are ignored in project settings, with a
warning. Other project settings override user settings per key. The extension
passes the object as `initializationOptions` on `initialize`. The bridge
requires it to be a JSON object or absent; any other type gets `-32602
InvalidParams` on `initialize` and the bridge exits 1. Unknown keys are ignored
with a log line. The bridge removes the whole `initializationOptions` field
before forwarding `initialize` to Godot. Changes need "restart language
server".

| key | type | default | meaning |
|---|---|---|---|
| `godot_path` | string | unset | Godot binary. |
| `project_dir` | string | unset | Dir containing `project.godot`. Relative paths join the worktree root. |
| `lsp_port` | u16 | unset | Attach to an already running editor on this port instead of spawning. |
| `dap_port` | u16 | 6006 | DAP port of that editor. Only read when `lsp_port` is set. |
| `startup_timeout_s` | u32 | 600 | Deadline for Godot's ports. 0 disables the deadline only. |
| `project_diagnostics` | bool | true | Phase 3. Open every script so all files get diagnostics. |
| `diagnose_addons` | bool | false | Include `addons/` in project diagnostics. |
| `extra_args` | [string] | [] | Placed before the bridge's own flags on the Godot command line. Rejected with an error if any element is one of `--path`, `--editor`, `-e`, `--headless`, `--lsp-port`, `--dap-port`, `--display-driver`, `--audio-driver`, `--quit`, `--quit-after`. |

Transport of settings to the other subcommands:
- `dap`: environment variable `GODOT_BRIDGE_SETTINGS`, JSON. The extension
  fills it from `LspSettings::for_worktree("godot", worktree).settings`, so
  user and project settings merge the same way as for the LSP.
- `run`, `project-dir`, `open-editor`, `doc`: read Zed's user settings
  (`$XDG_CONFIG_HOME/zed/settings.json`, default `~/.config/zed/`) and
  `<worktree>/.zed/settings.json`, both as JSON with comments (`json5` or a
  comment-stripping parser), path `lsp.godot.settings` in each. Merge per
  key, project over user. A missing file means no contribution. A parse or
  validation error names the file and the command exits 1.

### Worktree root

For `lsp`, from Zed's `initialize` params:
1. `workspaceFolders`, when present: the first entry whose URI has the `file`
   scheme. Non-file entries are skipped. If the array is present and has no
   file entry, error.
2. Else `rootUri`. Error when not a `file` URI.
3. Else `rootPath`.
4. Else cwd.

File URIs must have an empty host or `localhost`. Percent-decode the path,
then canonicalize (symlinks resolved). Error uses `-32002` with message
"godot-bridge: cannot determine a local worktree root from initialize
params" and exits 1.

For `dap`, `run`, `project-dir`, `open-editor`, `doc`: cwd, which Zed sets
to the worktree root, canonicalized.

### Project dir

Inputs in order. The first input that yields exactly one dir containing
`project.godot` wins. An input that yields zero moves to the next, except a
configured `project_dir`, which errors at once when invalid. An input that
yields several errors at once.

1. `--file <path>`: walk up from the file's dir to the filesystem root.
2. `project_dir` setting. Joined to the worktree root when relative. Must
   contain `project.godot`, else error "project_dir <p> has no project.godot".
3. The worktree root itself.
4. Breadth-first scan below the worktree root, depth 1 to 3. Skip dirs named
   `.git`, `.godot`, `addons`, `node_modules`, `target`, and any dir whose
   name starts with `.`. Do not follow symlinked dirs. Unreadable dirs are
   skipped with a log line.

Zero hits overall: `initialize` error `-32002` "No project.godot found under
<root>. Set lsp.godot.settings.project_dir." Several hits: `-32002` "Several
Godot projects under <root>: <a>, <b>. Set lsp.godot.settings.project_dir."
Both exit 1 after responding. Zed shows the message in the LSP log and the
status bar.

The project key is the canonical absolute path. Its identity hash is the
first 16 hex chars of `blake3(path)`.

### Godot binary

1. `godot_path` setting. 2. `GODOT` env var. 3. `godot4`, then `godot` on
PATH. Error "No Godot binary. Set lsp.godot.settings.godot_path, or GODOT,
or put godot on PATH." The binary must answer `--version` (5 s timeout) with
output starting `4.`, else error "Godot at <p> is not 4.x: <output>".

### Runtime dir and files

Runtime dir `$XDG_RUNTIME_DIR/godot-bridge/`, fallback `/tmp/godot-bridge-$UID/`,
created mode 0700. Four files per project, three roles. `<hash>` is the FNV
hash of the project path followed by `-` and the path length in bytes. The
lock is ownership. The socket is readiness. The state file is information only and
never used for synchronization.

- `<hash>.lock`: `flock(LOCK_EX | LOCK_NB)`, held by the `lsp` owner for its
  whole life.
- `<hash>.sock`: Unix socket the owner listens on. Bound on a private temporary
  path, chmod 0600, then atomically renamed into place after the lock is taken.
  Removed before the lock is released. Protocol: newline-delimited JSON, one
  request per line, one response per line, 5 s timeout per request, any number of concurrent
  clients. Requests: `{"cmd":"status"}`, `{"cmd":"handoff"}` (phase 4).
  Unknown cmd: `{"error":"unknown cmd"}`.
- `<hash>.json`: state, tmpfile+rename, mode 0600.
- `<hash>.dap.lock`: `flock` held by one `dap` session at a time.

Status response, also the state file's content with the extra fields:

```json
{"version":1,"project":"/abs/canonical","status":"starting|ready|recovering",
 "mode":"headless|gui|unmanaged",
 "godot_pid":null|N,"godot_pgid":null|N,"lsp_port":null|N,"dap_port":null|N,
 "owner_pid":null|N,"owner_start_ticks":null|N,"godot_start_ticks":null|N,
 "started_at":"rfc3339","bridge_version":"x.y.z"}
```

`godot_*` and `*_port` are null while `starting` before the spawn, integers
after. `owner_pid` and `owner_start_ticks` are null together and only for a
detached GUI state (phase 4). `*_start_ticks` is field 22 of `/proc/<pid>/stat`; a pid with
different ticks is a different process. `status` and `lsp` remove a state
file whose `owner_pid` is dead or ticks-mismatched; `lsp` does this while
holding the lock.

### `lsp` startup

1. Read Zed's `initialize`. Validate settings, resolve worktree root and
   project dir. Any error: respond with the error, exit 1.
2. `lsp_port` set: skip steps 3 to 7. Do not resolve a Godot binary.
   Connect to `127.0.0.1:<lsp_port>` with a 5 s timeout, mode `unmanaged`.
   No lock, socket, or state file.
3. Take `<hash>.lock`. On failure respond `-32002` "Another Zed window
   already serves <project>. Godot serves one client at a time." Exit 1.
4. Stale cleanup. Read `<hash>.json`. Mode `gui`: see phase 4; in phase 1
   leave the process alone and remove only the state and socket files. Else
   if `godot_pid` is alive with matching `godot_start_ticks`, it is an
   orphan of a dead owner. SIGTERM its `godot_pgid`, wait 5 s, SIGKILL,
   wait until the pid is gone. Never adopt a headless orphan. Remove the
   state file and any socket file.
5. Resolve the Godot binary (error: respond, cleanup, exit 1). Bind the
   socket. Publish status `starting`.
6. Spawn loop. Compute one monotonic deadline `now + startup_timeout_s`
   before the first spawn (none when 0). At most 3 spawned children total.
   Each attempt: pick free ports by bind-and-close, LSP in 6005..6999, DAP in
   7005..7999. Spawn Godot (see Lifetime). Write state. Poll every 200 ms:
   child exited, deadline passed, or LSP port accepts a TCP connect.
   - Child exited: log its last 20 lines, next attempt.
   - Deadline passed: terminate and reap the child, respond `-32002`
     "Godot did not start within <n>s. Last output: <20 lines>", cleanup,
     exit 1.
   - Attempts exhausted: same response with "Godot exited 3 times", cleanup,
     exit 1.
   - LSP port accepts: keep that connection for the session. Then connect
     and close the DAP port once; the loop for that also honors the
     deadline and child exit.
7. Publish `ready`.
8. Forward `initialize` (options stripped) to Godot. Wait for the response.
   Verify the `gdscript_client/changeWorkspace` path equals the project dir;
   mismatch (only possible in `unmanaged`) responds `-32002` "Editor on
   <port> serves <other>, expected <project>", exit 1. Patch the response
   (phase 3 adds `workspaceSymbolProvider: true`). Return it to Zed.
   Proxy from here on.

### LSP proxy

Framing to Zed and to Godot: exactly one header line
`Content-Length: N\r\n\r\n`. Caps: Zed-input decoder 64 MiB; Godot-input
decoder 64 MiB; anything written to Godot at most 4 MiB (Godot's limit);
Zed-output writer 64 MiB. A Zed request over 4 MiB gets `-32803` "message
too large for Godot"; a Zed notification over 4 MiB is dropped with a log
line; a Zed response over 4 MiB is a fatal client protocol error: log,
cleanup, exit 1. Malformed frame from Zed: log, cleanup, exit 1. Godot input
over 64 MiB, a malformed Godot frame, or unexpected TCP EOF from Godot:
treated as a Godot crash (see Crash recovery).

The proxy sees every message and keeps:
- Zed's `initialize` params (stripped) and Godot's response.
- `open_docs: key -> { uri, version, text, owner: Zed | Bridge }` keyed by
  canonical absolute path. Every file URI from Zed and every watcher path
  passes through the shared path-to-URI function before lookup, storage,
  or forwarding, so two spellings of one file map to one entry. The stored
  `uri` is what gets forwarded. On a successful Zed `didOpen` the bridge
  also records `incoming uri -> key`; later `didChange`/`didClose` for that
  incoming URI use the recorded key, so a deleted file (which no longer
  canonicalizes) still finds its entry. Likewise a successful scan or
  watcher create records `normalized absolute watcher path -> key`, and a
  watcher removal looks up that key without canonicalizing the missing
  file. A rename is a removal followed by a create. Mappings and the entry
  go away when the document is finally forgotten. Zed uses full sync; the bridge asserts
  `contentChanges` has one entry with no `range`, else logs and forwards
  as-is.
- Request ids. Every Zed request forwarded to Godot gets a bridge-owned
  integer id from one counter; the bridge keeps `bridge_id -> (zed_id,
  method, internal: bool)`. Responses from Godot are mapped back to the Zed
  id, or consumed when `internal`. Bridge-internal requests draw from the
  same counter with `internal: true`. `$/cancelRequest` from Zed is
  translated through the map.
- In-flight cap toward Godot: at most 32 outstanding requests. Extra Zed
  requests queue in order.

Versions: the bridge owns document versions sent to Godot. It rewrites the
`version` of every forwarded `didOpen`/`didChange` with a per-URI counter
starting at 1.

`$/cancelRequest` from Zed is forwarded, translated, only when the target
request was already sent to the current Godot connection. Otherwise the
queued request is dropped and Zed gets `-32800 RequestCancelled`.

Shutdown: `shutdown` request forwarded, response returned. On `exit`
notification or stdin EOF: close the TCP socket, kill the editor (unless
`unmanaged`), remove socket and state file, release the lock, exit 0.

### Lifetime of the editor

Godot spawns via `pre_exec` with `setpgid(0, 0)`, then
`prctl(PR_SET_PDEATHSIG, SIGTERM)`, then `getppid()` compared to the parent
pid captured before fork; a mismatch means the parent died first and the
child exits 1. Command line: `<godot> [extra_args] --editor --headless
--path <project> --lsp-port <p> --dap-port <q>`. Stdout and stderr are
pipes. A bridge task copies them to `<runtime dir>/<hash>.godot.log`, keeps
the last 20 lines in memory, and rotates the file to `.1` at 20 MiB. The
bridge records pid, pgid, start ticks.

Kill: SIGTERM to the process group, wait up to 5 s, SIGKILL the group,
then reap. The bridge tracks whether it is the parent of the current Godot
process. Own child: `waitpid`. Adopted detached GUI (phase 4): verify pid
start ticks, signal the recorded pgid, wait until the pid is gone, never
`waitpid`. Games launched by the editor are in that group and die with it.
Accepted for v1.

### Crash recovery (phase 1)

Active only after Godot's first `initialize` response was returned to Zed.
Before that point a child exit, malformed input, or TCP EOF consumes one
more attempt from the startup budget and deadline, respawns Godot, and
re-forwards the original `initialize`. On exhaustion, respond to Zed's
`initialize` with the startup `-32002` error and exit 1.

Trigger: child exit observed, or malformed frame or TCP EOF from Godot. On
a frame or EOF trigger with the child still alive, kill and reap it first.

1. Publish status `recovering`. Send `window/showMessage` type Error:
   "Godot exited (code N), restarting.".
2. Fail every pending Zed request with `-32803 RequestFailed`. Mark every
   outstanding server-to-client request id from the dead connection stale.
   Zed messages received from now on go into one FIFO tagged request,
   notification, or response, preserving arrival order, with request and
   notification counts capped at 1000 each. Overflow: a new request gets
   `-32803` at once; a new notification drops the oldest queued notification
   with a log line. When flushing, a response to a stale id is dropped; a
   response is forwarded only when its request came from the current
   connection.
3. Run the spawn loop with a fresh deadline and attempt budget. On failure:
   showMessage Error with the reason, cleanup, exit 1.
4. Replay `initialize` with a bridge-internal id, consume the response.
   Send `initialized` only if Zed's `initialized` had been forwarded before
   the crash; otherwise the queued Zed notification will supply it. Any
   server-to-client request during replay is answered with `null`.
   Project-wide work (phase 3) starts when `initialized` reaches Godot,
   once per Godot connection.
5. Replay `didOpen` for every `open_docs` entry with its current text and a
   fresh version.
6. Publish `ready`. Flush the FIFO in order. Resume proxying.

Three recoveries within 60 s: showMessage Error "Godot keeps crashing, see
<log path>", cleanup, exit 1.

### DAP proxy (phase 2)

Framing: same codec, one `Content-Length` header, 64 MiB cap both ways.
Malformed or oversized client input: exit 1 without a response. Every
message written to the client carries a bridge-owned monotonically
increasing `seq` starting at 1, forwarded Godot messages included. For
requests that Godot sends to the client, the bridge keeps a map from its
rewritten `seq` to Godot's original and restores the original in the
client's `request_seq` when forwarding the response. Client-to-Godot
messages keep the client's `seq` unchanged.

`godot-bridge dap`:
1. Read the client's `initialize` request first and hold it. Keep reading
   client messages concurrently during the steps below and buffer them in
   arrival order, combined cap 64 MiB; over the cap: exit 1. Every failure
   below responds `{ type: response, request_seq, command: initialize,
   success: false, message }`, then exits 1.
2. Resolve worktree root (cwd), settings (`GODOT_BRIDGE_SETTINGS`), project
   dir (`--file`, then settings, then root, then scan).
3. Take `<hash>.dap.lock` non-blocking, always. Failure: "A debug session
   for <project> is already running".
4. `lsp_port` set in settings: connect straight to `dap_port`, skip 5.
5. Find the owner through `<hash>.sock`. No socket or connection refused:
   "No Godot language server runs for <project>. Open a .gd file of the
   project in Zed first." Status `starting` or `recovering`: poll every
   500 ms until `ready`, with a monotonic deadline of `startup_timeout_s`
   (no deadline when 0), failing at once if the socket disappears or
   refuses. Then connect to `dap_port` and keep the connection.
6. Forward the held `initialize`, then flush the buffer in order.
7. Rewrite `launch` and `attach` arguments: remove `adapter`, `request`,
   `file`, and any `project`. For `launch` set `project` to the canonical
   project dir and resolve `scene`:
   - `main` or absent: `main`.
   - `current`: requires `--file`. Without it respond to the `launch`
     request with `success: false`, message "scene current requires
     --file". With a `.tscn` file: that file. With any other file: scene
     resolution below. No match: `success: false`, "No scene uses <file>".
   - Any other string: pass through unchanged.
8. Pass everything else through, with one filter: Godot emits `exited` and
   `terminated` synchronously while handling `configurationDone`, before the
   new game's `process` event and the `launch` response. Zed ends the session
   on `terminated`. The bridge drops `exited` and `terminated` events until a
   `process` event has been seen on the connection. Godot dies or the TCP socket closes:
   send `terminated` then `exited` events with fresh `seq`, close stdout,
   exit 1. Client stdin EOF: close TCP, release the lock, exit 0.

Scene resolution for a script at `<project>/rel/path.gd`:
1. `<project>/rel/path.tscn` if it exists.
2. Else walk all `.tscn` files under the project (skip `.godot/`, `addons/`,
   hidden dirs, no symlinks) in lexicographic order of their relative path.
   Parse each `[ext_resource ...]` header line; the first file with a
   `path="res://rel/path.gd"` attribute wins. Comparison is exact after
   normalizing both sides to `res://` form.
3. Return `res://<relative scene path>`.

### Project-wide diagnostics (phase 3)

`project_diagnostics` true: on initial startup after Zed's `initialized`
notification has been forwarded to Godot, and after each recovery step 5,
the bridge lists every `.gd` under the project (skip `.godot/`,
`addons/` unless `diagnose_addons`, hidden dirs, no symlinks) and for each
URI not already in `open_docs` sends `didOpen` with disk content, owner
`Bridge`, 20 files per 50 ms. URIs are percent-encoded canonical `file://`
URIs, built with one function used everywhere. Files over 2 MiB or not
valid UTF-8 are skipped with a log line, in the scan and in the watcher.

One task owns `open_docs` and serializes Zed events and watcher events.
- Zed `didOpen` on a Bridge-owned URI: forward as `didChange` with the new
  text, owner becomes `Zed`.
- Zed `didClose`: drop the URI's cached symbols and invalidate its
  outstanding symbol requests, then forward `didClose`. If the file exists
  on disk, `didOpen` again from disk, owner `Bridge`, cache empty until a
  `documentSymbol` response for the new version arrives. Else forget it.
- Watcher (`notify`, 300 ms debounce, then recheck ownership): an eligible
  URI created and absent from `open_docs`: `didOpen` from disk, owner
  `Bridge`. A Bridge-owned URI modified: `didChange` from disk. A
  Bridge-owned URI removed: `didClose`, drop cached symbols, and send Zed
  `publishDiagnostics` with an empty array for that URI. Zed-owned URIs:
  ignored.
- Godot `publishDiagnostics`: pass through unchanged.
- During recovery (status `recovering`) watcher creates and modifications
  update Bridge-owned `open_docs` text but send nothing to Godot and
  schedule no symbol requests. A Bridge-owned removal deletes the entry and
  cached symbols at once and sends Zed an empty `publishDiagnostics`; no
  `didClose` goes to the disconnected Godot. Recovery step 5 replays the
  resulting state. Watcher output resumes after `ready`. Zed traffic stays
  governed by the recovery FIFO.

### Workspace symbols (phase 3)

Bridge patches the `initialize` response with `workspaceSymbolProvider:
true`. No symbol requests are sent before Zed's `initialized` has been
forwarded. For each doc in `open_docs`, after open and after each change,
debounced 300 ms, the bridge sends `textDocument/documentSymbol` with a
bridge-internal id, tagged with the doc's version. A response is consumed
only when the URI is still in `open_docs` at that same version and was not
closed or removed since the request was issued; otherwise it is discarded.
In-flight cap counts these requests too.

Cache per URI: flattened list of `{ name, kind, container, uri, range }`.
Nested `DocumentSymbol`: use `selectionRange` as `range`; `container` is
ancestor names joined with `.`. `SymbolInformation[]` results are taken
as-is after URI normalization.

`workspace/symbol` answers from the cache without contacting Godot:
case-folded subsequence match of the query against `name`. A name can match
in several alignments; pick the one with the lexicographically smallest
tuple (total gap between matched characters, first match offset, matched
index sequence). Rank by that gap ascending, then that offset ascending,
then name, then URI, then range start. Truncate to 200. For an empty query every symbol
matches with gap 0 and offset 0, so the order is name, URI, range start.
With `project_diagnostics` false, only Zed-open files are covered. In that
case, right after returning the initial `initialize` response, the bridge
sends `window/showMessage` type Info: "Project diagnostics are disabled;
workspace symbols cover only files open in Zed." Sent once per bridge
process, not repeated after recovery.

### GUI hand-off (phase 4)

`godot-bridge open-editor --file <path>`:
- Resolves project and settings. Connects to `<hash>.sock`, sends
  `{"cmd":"handoff"}`, prints the response, exits 0 on `accepted:true`,
  1 otherwise.
- No socket or refused: take `<hash>.lock` non-blocking. Lock held by
  someone else: retry socket discovery for 5 s, then exit 1 with "An owner
  exists but does not answer". Lock taken: if a detached `gui` state has a
  live pid with matching ticks, wait for its recorded ports with the
  startup deadline; ports up: print the state, exit 0; deadline: exit 1
  with "GUI editor <pid> is not answering on its ports", launch nothing.
  No live GUI: remove stale state, pick LSP and DAP ports, launch `<godot>
  [extra_args] --editor --path <project> --lsp-port <p> --dap-port <q>`
  detached (`setsid`, stdio to `<runtime dir>/<hash>.gui.log`), write
  `mode: gui`, status `starting`, null owner fields at once, wait for both
  ports with the startup deadline, set status `ready`, release the lock,
  print the state, exit 0. On child exit or deadline: kill and reap its
  process group, remove the state, exit 1 with the last 20 output lines.

Owner handling `handoff`:
- Try to take `<hash>.dap.lock` non-blocking and hold it through the whole
  swap; release it when the GUI reaches `ready`, or when the fallback
  headless recovery reaches `ready` or the bridge exits. Reject
  synchronously with `accepted:false` and a reason when that lock is held
  by someone else, when status is `starting` or `recovering`, or when mode
  is `unmanaged`.
- Mode already `gui`: respond `accepted:true`, no action.
- Else respond `{"version":1,"accepted":true}` at once, then asynchronously:
  1. Fail pending Zed requests with `-32803`, queue new traffic as in
     recovery step 2, publish `recovering`.
  2. Kill headless Godot, wait until both ports refuse connections.
  3. Spawn `<godot> [extra_args] --editor --path <project> --lsp-port <p>
     --dap-port <q>` with the same ports, in a new session (`setsid`), no
     PDEATHSIG, stdio to `<hash>.gui.log`. Mode `gui`. Record pid, pgid,
     ticks in state.
  4. Wait for both ports with a fresh deadline. Connect and retain the LSP
     socket. Run recovery steps 4 to 6 (replay, flush, `ready`).
  5. Failure at any step: showMessage Error, kill and reap the new GUI's
     process group, then run normal crash recovery, which spawns headless
     again.
- The owner never kills a `gui` child, not at shutdown either. At shutdown
  in `gui` mode the state file is kept with `owner_pid` and
  `owner_start_ticks` set to null (the only case where they are null) and
  the socket removed. A later `lsp` on that project, after taking the lock,
  finds `mode: gui` with no live owner: if `godot_pid` is alive with
  matching ticks, it keeps owner fields null, waits for both recorded ports
  with the startup deadline, then binds the socket, writes its own owner
  identity, keeps ports and mode `gui`, and connects without killing.
  Deadline passed: `-32002` "GUI editor <pid> is not answering on its
  ports", state left as detached GUI with null owner fields, no socket,
  exit 1, GUI left alone. Dead pid: remove the state and run normal
  headless startup. Phase 1 implements the check as "mode gui: never kill";
  phase 4 adds the reconnect.
- GUI exit (child exit observed): exactly one normal crash recovery, which
  returns to headless. This exit does not count toward the 3-in-60 s limit.

### Doc lookup

`godot-bridge doc <symbol>`: opens
`https://docs.godotengine.org/en/stable/classes/class_<lower>.html` with
`xdg-open`. `Class.member` adds `#class-<class>-method-<member>`; if the
page has no such anchor the browser lands on the class page, which is
accepted.

## Extension: `godot`

Written from scratch. Extension id `godot`. `extension.toml` declares the
three grammars and one language server in phase 1, and the debug adapter
only from phase 2 (T2.4). No auto-download in v1.

Grammars, pinned (HEAD on 2026-09-04):

| grammar | repository | commit |
|---|---|---|
| `gdscript` | https://github.com/PrestonKnopp/tree-sitter-gdscript | `c5c8fa4861b5a4f04a7e60d97587fc3b6cc5639e` |
| `godot_resource` | https://github.com/PrestonKnopp/tree-sitter-godot-resource | `302c1895f54bf74d53a08572f7b26a6614209adc` |
| `gdshader` | https://github.com/GodOfAvacyn/tree-sitter-gdshader | `14e834063e136fa69b6d91f711f4f1981acf424b` |

Languages (`languages/<dir>/config.toml`):

| dir | name | suffixes | comments | other |
|---|---|---|---|---|
| `gdscript` | `GDScript` | `gd` | line `# `, `## ` | `hard_tabs = true`, `increase_indent_pattern = ":\\s*$"`, `autoclose_before = ":.,}])>"`, brackets `[] () {} "" ''`, `debuggers = ["godot"]` from phase 2 |
| `gdshader` | `GDShader` | `gdshader`, `gdshaderinc` | line `// `, block `/* */` | brackets `{} [] ()` |
| `godot_resource` | `Godot Resource` | `tscn`, `tres`, `godot`, `import`, `gdextension` | line `; ` | brackets `[] ()` |

Queries per language, written fresh against the pinned grammar's
`src/node-types.json`: `highlights.scm` (required captures: `@keyword`,
`@function`, `@type`, `@string`, `@number`, `@comment`, `@variable`,
`@operator`, `@punctuation.bracket`, `@property`, `@constant`),
`brackets.scm`, `indents.scm` (gdscript only: `@indent` on block bodies,
`@end` on dedent tokens), `outline.scm` (gdscript: class, function, signal,
variable; gdshader: function, uniform; resource: section headers),
`injections.scm` (gdscript: none needed; resource: none), `textobjects.scm`
(gdscript: `@function.around/inside`, `@class.around/inside`). Acceptance:
Zed loads the extension with no query errors in its log, and each of
`fixtures/minimal-project/main.gd`, `main.tscn`, and a sample `.gdshader`
highlights keywords and strings.

The installed GDQuest `gdscript` extension claims the same suffixes.
`docs/dev-setup.md` says to uninstall it first.

### Language server

`language_server_command`:
1. `lsp.godot.binary.path` setting.
2. `worktree.which("godot-bridge")`.
3. Error "Install godot-bridge: cargo install --path bridge".

Args `["lsp"]`, then `["--"] + binary.arguments` when set. Env:
`worktree.shell_env()` so `GODOT` and PATH match the user's shell.

`language_server_initialization_options` returns
`LspSettings::for_worktree("godot", worktree).settings` unchanged, or `None`.

### Debug adapter (phase 2)

`[debug_adapters.godot]` with `debug_adapter_schemas/godot.json`:
properties `request` (enum launch|attach, required), `scene` (string),
`file` (string), `additionalProperties: false`.

- `dap_request_kind`: from `request`; missing or other value is an error.
- `get_dap_binary`: parse `DebugTaskDefinition.config` as JSON. Return
  `DebugAdapterBinary { command: <bridge path, same resolution as LSP>,
  arguments: ["dap"] + ["--file", file] when present + ["--"] +
  binary.arguments when set, cwd: worktree root, envs: [GODOT_BRIDGE_SETTINGS
  = serialized settings object or "{}"], connection: None, request_args: {
  configuration: the config JSON string, request: from dap_request_kind } }`.
- `dap_config_to_scenario`: for the "new session" modal. `DebugRequest::
  Launch`: `scene` is `LaunchRequest.program` when it ends `.tscn`, else
  `main`; config `{ "request": "launch", "scene": ... }`. `DebugRequest::
  Attach`: config `{ "request": "attach" }`, `process_id` ignored because
  Godot attaches through the project's DAP endpoint.

Sample `docs/sample/.zed/debug.json`:

| label | config |
|---|---|
| Godot: launch main scene | `{ "adapter": "godot", "request": "launch", "scene": "main" }` |
| Godot: launch scene of current file | `{ "adapter": "godot", "request": "launch", "scene": "current", "file": "$ZED_FILE" }` |
| Godot: attach | `{ "adapter": "godot", "request": "attach" }` |

Gate T0.3 verifies that Zed substitutes `$ZED_FILE` inside debug.json on
1.17.2. If it does not, the "scene of current file" template is removed
from the sample and the task list, and `run --scene current` remains the
way to run the current scene.

### Tasks

Sample `docs/sample/.zed/tasks.json`, all `command` + `args`:

| label | args |
|---|---|
| godot: run project | `godot-bridge run --file $ZED_FILE` |
| godot: run current scene | `godot-bridge run --file $ZED_FILE --scene current` |
| godot: open editor | `godot-bridge open-editor --file $ZED_FILE` |
| godot: docs for symbol | `godot-bridge doc $ZED_SYMBOL` |
| godot: bridge status | `godot-bridge status` |

## Out of scope for v1

macOS, Windows, Godot 3.x, two Zed windows on one project, two debug
sessions on one project, debugging without the LSP running, auto-download
of bridge or Godot, extension registry publishing.

## Verification

Unit, `bridge/`:
- root: fixtures nested, two projects, none, `project_dir` override,
  symlinked dir skipped, `--file` walk-up.
- state: second lock rejected, stale state removed on dead owner, reused pid
  ignored via ticks, socket status round trip, nullable fields while
  starting.
- framing: split frames, two messages in one read, oversized, malformed,
  single-header output.
- open_docs and version rewriting, in-flight cap ordering, cancel rules.
- scene resolution: adjacent stem, ext_resource reference, lexicographic
  tie, none.
- symbol ranking order and truncation.

Integration, needs `/usr/bin/godot`, `fixtures/minimal-project`:
- `lsp`: initialize, didOpen with a type error, publishDiagnostics arrives.
  Close stdin, Godot pid gone within 6 s, lock and socket gone.
- `lsp` on `fixtures/nested`: resolves the nested project.
- `lsp` twice on the same project: second gets the "Another Zed window"
  error.
- crash: SIGKILL Godot mid-session, showMessage arrives, a completion
  request succeeds after recovery.
- `dap` without owner: initialize response `success: false` with the
  expected message. `dap` with owner: launch main, `process` event, the game
  pid exits on `terminate`, the owner stays `ready`.

Manual on `~/projects/DieQuest`: open a `.gd` in Zed with no Godot running,
completion works, project symbols work, breakpoint hit from "launch main
scene", `godot: open editor` opens GUI and completion keeps working, closing
GUI brings headless back.

## Gates (`docs/gates.md`)

- T0.3, before phase 2: Zed 1.17.2 substitutes `$ZED_FILE` inside debug.json
  config and `get_dap_binary` receives the substituted value.
- Measured 2026-09-04, phase 3 constants derived: bulk open 20 per 50 ms,
  documentSymbol debounce 300 ms, in-flight cap 32. Re-measure only if
  DieQuest symbol search feels slow.
