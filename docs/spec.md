# godot-bridge: spec

One native binary, `godot-bridge`, owns a headless Godot editor per project.
The editor client spawns it as the LSP (`godot-bridge lsp`) and once per
debug session as the DAP (`godot-bridge dap`). Both proxy stdio to the editor's TCP ports.

```
Editor ──stdio──► godot-bridge lsp ──tcp──► Godot editor (headless)
Editor ──stdio──► godot-bridge dap ──tcp──►   --lsp-port, --dap-port
                      └── owns, kills, respawns ──┘
```

The Zed extension (wasm) cannot spawn processes, so the bridge owns Godot.
Godot facts the design rests on: Godot LSP serves one client, uses full text sync, accepts one
`Content-Length` header and frames up to 4 MiB, has no `workspace/*`
methods, publishes diagnostics only for opened documents, and can crash
under load. `bridge/` depends on `libc` at runtime; `serde_json` is a
dev-dependency only.

## Subcommands

| command | purpose |
|---|---|
| `lsp` | The editor's LSP entry. Owns the Godot editor. |
| `dap [--file <path>]` | DAP proxy. Needs a running `lsp` owner. |
| `project-dir --file <path>` | Prints the canonical project dir. |
| `run --file <path> [--scene <value>]` | Runs `<godot> [extra_args] --path <project> [scene]`. `--scene current` runs a `.tscn` directly or resolves the scene of a script. |
| `open-editor --file <path>` | Swaps the headless editor for a GUI editor on the same ports. |
| `status` | One JSON status per project with a responding socket. Removes stale state files. |
| `doc <symbol>` | Opens the class reference in the browser. `Class.member` adds `#class-<class>-method-<member>`, underscores as hyphens. |

`--` and anything after it is ignored. Unknown flags and stray arguments
exit 1. Errors print a message and exit 1.

## Settings

For Zed, `lsp.godot.settings` in user settings or `.zed/settings.json`. `godot_path`,
`project_dir` and `extra_args` are ignored in project settings with a
warning. Other project keys override user keys. The Zed extension passes the
merged object as `initializationOptions`; the bridge strips it before
forwarding `initialize` to Godot. A non-object gets `-32602`, exit 1.

| key | default | meaning |
|---|---|---|
| `godot_path` | unset | Absolute, executable. |
| `project_dir` | unset | Dir with `project.godot`, relative to the workspace root. |
| `lsp_port` | unset | Attach to a running editor, mode `unmanaged`. No lock, socket, state or spawn. |
| `dap_port` | 6006 | With `lsp_port` only. |
| `startup_timeout_s` | 600 | Deadline for ports and the `initialize` response. 0 disables. |
| `project_diagnostics` | true | Open every script so all files get diagnostics. |
| `diagnose_addons` | false | Include `addons/`. |
| `extra_args` | [] | Before the bridge's flags. Rejected: `--path`, `--editor`, `-e`, `--headless`, `--lsp-port`, `--dap-port`, `--display-driver`, `--audio-driver`, `--quit`, `--quit-after`, `--script`, `-s`, `--main-pack`, `--export-release`, `--export-debug`, `--export-pack`. |

Other editor clients set `GODOT_BRIDGE_SETTINGS` to one JSON object with the
same keys, already merged and trusted by the client. When it is set no
settings file is read, `initializationOptions` is ignored, and untrusted
keys are not stripped. Without it, `lsp` merges `initializationOptions`
over the user file and the other commands read
`$XDG_CONFIG_HOME/zed/settings.json`, else `~/.config/zed` and on
Windows `%APPDATA%\Zed` (neither set: skipped), and
`<worktree>/.zed/settings.json` as JSON with comments, each at most 1 MiB, a
regular file, opened without following symlinks. Missing files contribute
nothing. Parse or validation errors name the file.

## Roots

Worktree root for `lsp`: first `file` entry of `workspaceFolders`, else
`rootUri`, else `rootPath`, else cwd. Host must be empty or `localhost`.
Percent-decoded, canonicalized. On Windows a URI is `file:///C:/x` with the
drive upper-cased, `file:///c%3A/x` decodes the same, UNC is refused, the
`\\?\` prefix is stripped before hashing or display, and paths compare
case-insensitively. Failure: `-32002`, exit 1. Other commands
use cwd. A single-file worktree has no project settings.

Project dir, first input yielding exactly one `project.godot` wins, several
is an error: `--file` walk-up, `project_dir` (invalid errors at once), the
root, then a breadth-first scan to depth 3 skipping `.git`, `.godot`,
`addons`, `node_modules`, `target`, hidden dirs and symlinks. Zero hits:
`-32002` "No project.godot found under <root>. Set
the project_dir setting."

Godot binary: `godot_path`, `GODOT` (absolute, executable), then `godot4`,
`godot` on PATH. Must answer `--version` within 5 s with `4.`. On Windows the
PATH search appends `PATHEXT`, then `%LOCALAPPDATA%\Programs\Godot`, the
WinGet store and Steam are searched, and the plain `.exe` is preferred over
its `_console` launcher, which would put the engine in a child process.

## Runtime files

`$XDG_RUNTIME_DIR/godot-bridge/`, fallback `/tmp/godot-bridge-$UID/`, 0700,
verified on every use. Files 0600, opened `O_NOFOLLOW`. On Windows the
directory is `%LOCALAPPDATA%\godot-bridge`, fallback `%TEMP%\godot-bridge`,
created with the protected DACL
`D:P(A;OICI;FA;;;<user>)` and verified on every use: an existing directory is
refused, never repaired, unless its owner and every access-allowed entry name
the current user, `SYSTEM` or `BUILTIN\Administrators`. Reads refuse reparse
points instead of `O_NOFOLLOW`. `<hash>` is FNV-1a of the canonical project
path, `-`, path length.

- `<hash>.lock`: `flock`, on Windows `LockFileEx` exclusive and immediate,
  held by the `lsp` owner for its life. Ownership.
- `<hash>.sock`: owner's Unix socket, bound on a temp path and renamed in
  after the lock; on Windows a named pipe `\\.\pipe\godot-bridge-<hash>`
  whose security descriptor grants the current user only. Readiness. Newline
  JSON, 5 s per request, 16 clients, 30 s idle. `{"cmd":"status"}`,
  `{"cmd":"handoff"}`, else `{"error":"unknown cmd"}`.
- `<hash>.json`: state, tmpfile and rename. Information only.
- `<hash>.dap.lock`: one `dap` session at a time.
- `<hash>.godot.log`, `<hash>.gui.log`: Godot output, rotated to `.1` at 20 MiB.

State and status object:

```json
{"version":1,"project":"/abs","status":"starting|ready|recovering",
 "mode":"headless|gui|unmanaged","godot_pid":N,"godot_pgid":N,
 "lsp_port":N,"dap_port":N,"owner_pid":N,"owner_start_ticks":N,
 "godot_start_ticks":N,"started_at":"rfc3339","bridge_version":"x.y.z"}
```

Numbers are null before the spawn. Owner fields are null only for a detached
GUI. `*_start_ticks` is `/proc/<pid>/stat` field 22, on Windows the
`GetProcessTimes` creation time; a pid with other ticks is another process. A
state file whose owner is dead is removed.

## `lsp` startup

1. Read `initialize`, validate settings, resolve roots. Error: respond, exit 1.
2. Take the lock. Held: `-32002` "Another editor window already serves
   <project>. Godot serves one client at a time."
3. Stale cleanup: a live headless orphan (pid and ticks match) is killed by
   group. `mode: gui` with a live pid: wait for its ports under the deadline,
   bind the socket, adopt without killing. Deadline: `-32002` "GUI editor
   <pid> is not answering on its ports", exit 1.
4. Bind the socket, publish `starting`.
5. Spawn loop: one deadline, at most 3 children. Free ports by bind-and-close,
   LSP 6005..6999, DAP 7005..7999. Poll every 200 ms for child exit (log 20
   lines, next attempt), deadline (`-32002` "Godot did not start within <n>s.
   Last output: ..."), or LSP port accepting. Then connect and close the DAP
   port once.
6. Publish `ready`. Forward `initialize` under the deadline (timeout:
   `-32002` "Godot did not answer initialize within <n>s"). Verify Godot's
   `changeWorkspace` path is the project dir. Add
   `workspaceSymbolProvider: true`. Return the response. Proxy.

Spawn: `pre_exec` with `setpgid(0,0)`, `PR_SET_PDEATHSIG`, parent check; on
Windows a job object with `KILL_ON_JOB_CLOSE`, which the GUI editor breaks
away from before joining a named job of its own,
`Local\godot-bridge-<pid>-<ticks>`, with no `KILL_ON_JOB_CLOSE` and a
current-user descriptor. `<godot> [extra_args] --editor --headless --path
<project> --lsp-port <p> --dap-port <q>`, stdio piped to the log, last 20
lines kept. Kill: SIGTERM the group, 5 s, SIGKILL the group, reap; on Windows
`TerminateJobObject`, and for a recorded GUI editor the named job reopened
from the state file, falling back to a recursive walk of the descendants when
there is none. Games the editor launches die with the group.

## LSP proxy

Frames: one `Content-Length` header. 8 MiB per frame each way, 4 MiB written
to Godot. Client request over 4 MiB: `-32803`. Notification over 4 MiB:
dropped. Response over 4 MiB or malformed client frame: exit 1. Oversized or
malformed Godot frame, or TCP EOF: treated as a crash.

State: stripped `initialize` and Godot's response; `open_docs` keyed by
canonical path, at most 20 000, `{uri, version, text, owner: Editor|Bridge}`,
with one path-to-URI function for every editor URI and watcher path and a
recorded `incoming uri -> key` so deleted files still resolve; request ids
mapped `bridge_id -> (client_id, method, internal)`; 32 requests in flight
toward Godot, the rest queued in order. Forwarded `didOpen`/`didChange`
versions are rewritten from a per-URI counter. `$/cancelRequest` for a queued
request drops it with `-32800`.

Shutdown: forward `shutdown`, wait at most 5 s. On `exit` or stdin EOF:
close TCP, kill the editor unless `unmanaged`, remove socket and state,
release the lock, exit 0.

## Crash recovery

Before the first `initialize` response reached the editor, a crash consumes one
spawn attempt and re-forwards `initialize`. After:

1. `recovering`. `window/showMessage` Error "Godot exited (code N), restarting.".
2. Fail pending requests with `-32803`. Queue editor traffic in one FIFO,
   requests and notifications capped at 1000 each; overflow answers `-32803`
   or drops the oldest notification. Responses to the dead connection's
   requests are dropped.
3. Spawn loop with a fresh deadline and budget. Failure: showMessage, exit 1.
4. Replay `initialize` internally; `initialized` if it had been forwarded.
   Server requests during replay get `null`.
5. Replay `didOpen` for every open doc with a fresh version.
6. `ready`, flush the FIFO, resume.

Three recoveries in 60 s: showMessage "Godot keeps crashing, see <log>", exit 1.

## DAP proxy

Same framing, 8 MiB. Every message to the client gets a bridge `seq` from 1;
Godot-to-client requests keep a `seq` map so `request_seq` maps back.

1. Hold the client's `initialize`, buffer later messages (8 MiB). Failures
   below answer it with `success: false` and exit 1.
2. Resolve root, settings, project. Take `<hash>.dap.lock`: "A debug session
   for <project> is already running".
3. `lsp_port` set: connect to `dap_port`. Else find the owner via the socket
   ("No Godot language server runs for <project>. Open a .gd file of the
   project in your editor first."), poll every 500 ms until `ready` under the
   deadline, connect.
4. Forward `initialize`, flush the buffer.
5. `launch`/`attach`: drop `adapter`, `request`, `file`, `project`; set
   `project` to the project dir; `scene`: `main` or absent stays `main`,
   `current` needs `--file` ("scene current requires --file"), a `.tscn` is
   used directly, a script goes through scene resolution ("No scene uses
   <file>"), anything else passes through.
6. Drop `exited` and `terminated` until a `process` event was seen, Godot
   emits them early. Godot gone: send `terminated`, `exited`, exit 1. Client
   EOF: release the lock, exit 0.

Scene resolution for `rel/path.gd`: `rel/path.tscn` if it exists, else the
first `.tscn` in lexicographic order (skip `.godot/`, `addons/`, hidden,
symlinks) whose `[ext_resource ...]` line has `path="res://rel/path.gd"`.

## Project diagnostics and symbols

With `project_diagnostics`, after `initialized` reaches Godot and after each
recovery, every `.gd` (skip `.godot/`, `addons/` unless `diagnose_addons`,
hidden, symlinks) not already open gets `didOpen` from disk as `Bridge`, 100
per batch, next batch when their diagnostics arrive or after 1 s. Over 2 MiB
or invalid UTF-8: skipped.

Editor `didOpen` on a Bridge doc becomes `didChange`, owner `Editor`. Editor
`didClose` forwards, then reopens from disk as `Bridge` if the file exists.
Watcher (inotify, on Windows `ReadDirectoryChangesW` over the project subtree,
300 ms debounce): create opens, modify of a Bridge doc
changes, remove of a Bridge doc closes and clears the editor's diagnostics, editor
docs are ignored, moved directories rescan. While `recovering` the watcher
only updates text; recovery replays it. If the watch cannot be created the
session continues without it and logs a warning.

Symbols: after open and each change, debounced 300 ms, an internal
`documentSymbol` tagged with the version; stale responses are dropped.
Cache: flat `{name, kind, container, uri, range}` with `selectionRange` and
dotted ancestor names. `workspace/symbol`: case-folded subsequence match of
the query (max 256 bytes) on `name`, or on `container.name` when it has `.`
or a space. Rank by gap, offset, name, URI, range. Truncate to 200. With
`project_diagnostics` off, one Info showMessage says symbols cover open files
only.

## GUI hand-off

`open-editor`: send `handoff` over the socket, print the reply, exit 0 on
`accepted:true`. No socket: take the lock (held: retry 5 s, "An owner exists
but does not answer"). A live detached GUI: wait for its ports, print state.
Else launch `<godot> [extra_args] --editor --path <project> --lsp-port <p>
--dap-port <q>` detached (`setsid`, on Windows `CREATE_BREAKAWAY_FROM_JOB`,
stdio to `<hash>.gui.log`), `mode: gui`,
wait for ports, `ready`, release the lock, print state. Failure: kill the
group, remove state, exit 1 with the last 20 lines.

Owner on `handoff`: take `<hash>.dap.lock` for the swap. Reject when it is
held, status is not `ready`, or mode is `unmanaged`. Already `gui`: accept,
no action. Else accept, then: fail pending requests and queue as in
recovery, kill headless, wait for both ports to refuse, spawn the GUI on the
same ports (new session, no PDEATHSIG), wait for ports, connect, run recovery
steps 4 to 6. Any failure: showMessage, kill the GUI, normal recovery back to
headless.

The owner never kills a GUI child. At shutdown in `gui` mode the state stays
with null owner fields for the next `lsp` to adopt. GUI exit triggers one
recovery to headless that does not count toward the 3-in-60 s limit.

## Clients

Each editor ships a thin client that spawns the bridge. The Zed extension
(wasm; `extension.toml` pins three grammars, one language server, one debug
adapter) runs an absolute `lsp.godot.binary.path`, else `godot-bridge` from
PATH, with args `["lsp"]` and the shell env minus `GODOT_BRIDGE_SETTINGS`.
Relative paths are ignored; Zed applies project settings only for trusted
worktrees.
`LspSettings::for_worktree("godot").settings` becomes
`initializationOptions`. VS Code and Neovim pass their merged settings in
`GODOT_BRIDGE_SETTINGS` and take the bridge path from user settings only.

Debug adapter: the bridge runs as `["dap", "--file", file]` when a file is
known, cwd the project. Zed maps a `Launch` `program` ending in `.tscn` to
`scene`, else `main`; `Attach` ignores `process_id`. Zed substitutes
`$ZED_FILE` in `debug.json`. The `godot` debug locator accepts a
`godot-bridge run` task and returns a launch scenario with its `--scene` and
`--file` values, so the shipped `godot: run` tasks debug without a
`debug.json`.

GDScript indent: increase after `:`, `@indent` on bodies, `@start.<kw>`
captures so `else`/`elif` dedent to `if`, `elif`, `for`, `while`.

Out of scope: macOS, Godot 3.x, two windows or two debug sessions on one
project, auto-download.
