# Windows port

One new crate, `windows-sys`. Do the macOS port first so the `sys/` split
exists. JSON, framing, LSP and DAP logic, settings, symbols, TCP proxying and
the extension are unchanged.

| module | Unix | Windows | size |
|---|---|---|---|
| `watch.rs` | inotify | `ReadDirectoryChangesW` on the project root, `bWatchSubtree`, one thread | 250 lines |
| `state.rs` socket | Unix socket | named pipe `\\.\pipe\godot-bridge-<hash>`, current-user security descriptor | 200 |
| `state.rs` lock | `flock` | `LockFileEx` exclusive, fail immediately | 40 |
| `state.rs` runtime dir | `$XDG_RUNTIME_DIR` | `%LOCALAPPDATA%\godot-bridge`, default ACL | 40 |
| `state.rs` start ticks | `/proc` | `GetProcessTimes` creation time | 30 |
| `process.rs` spawn, kill | `setsid`, PDEATHSIG, `kill(-pgid)` | job object with `KILL_ON_JOB_CLOSE` for headless, breakaway for the GUI editor, `TerminateJobObject` | 150 |
| `process.rs` port owner | `/proc/net/tcp` | `GetExtendedTcpTable(TCP_TABLE_OWNER_PID_LISTENER)`, v4 and v6 | 60 |
| `O_NOFOLLOW` reads | | `symlink_metadata` check or `FILE_FLAG_OPEN_REPARSE_POINT` | 50 |
| `file_uri.rs`, `root.rs` | `file:///abs` | `file:///C:/x`, drive upper-cased as Godot sends it, case-insensitive compare, strip `\\?\` before hashing and display, refuse UNC | 80 |
| `godot_bin.rs` | PATH | `godot.exe`, `%LOCALAPPDATA%\Programs\Godot`, Steam. Use the `*_console.exe` variant for stdout. | 40 |
| `main.rs` | `mallopt` | remove under `cfg` | 3 |

Steps: `sys/windows.rs`, then paths and URIs with tests, lock and pipe,
job objects, watcher, port table, Godot discovery, full suite on Windows
including GUI hand-off and debugging.

Risks: antivirus holds saved files briefly, retry a failed read once; long
paths need `\\?\` internally.
