# Windows port

Estimate: one to two weeks for one person with a Windows machine, plus
verification. One new crate: `windows-sys` (bindings only, no code, generated
from metadata). The macOS port should land first, so the `sys/` split exists.

## What already works

The JSON parser, framing, LSP and DAP logic, settings, symbol search, TCP
proxying to Godot, and the extension. Roughly 80 percent of the bridge.

## Pieces to replace

| module | Unix mechanism | Windows replacement | size |
|---|---|---|---|
| `watch.rs` | inotify | `ReadDirectoryChangesW` on the project root with `bWatchSubtree`, overlapped I/O, one thread. Gives file paths and create, modify, delete, rename. | 250 lines, simpler than kqueue |
| `state.rs` owner socket | Unix domain socket | Named pipe `\\.\pipe\godot-bridge-<hash>` with a security descriptor limited to the current user, or `AF_UNIX` which Windows 10 1803 supports but Rust std does not expose. Named pipe is the safe choice. | 200 lines |
| `state.rs` lock | `flock` | `LockFileEx` with `LOCKFILE_EXCLUSIVE_LOCK` and `FAIL_IMMEDIATELY`. Same semantics, lock dies with the handle. | 40 lines |
| `state.rs` runtime dir | `$XDG_RUNTIME_DIR`, 0700 | `%LOCALAPPDATA%\godot-bridge`. Per user by default. Drop the mode checks, verify the owner SID instead or accept the default ACL. | 40 lines |
| `state.rs` `process_start_ticks` | `/proc/<pid>/stat` | `OpenProcess` plus `GetProcessTimes` creation time | 30 lines |
| `process.rs` spawn | `setsid`, `setpgid`, PDEATHSIG, `kill(-pgid)` | Job object with `KILL_ON_JOB_CLOSE` for headless. GUI hand-off needs the child outside the job, so use `CREATE_BREAKAWAY_FROM_JOB` or a separate job without kill-on-close. Kill with `TerminateJobObject`. | 150 lines |
| `process.rs` port ownership | `/proc/net/tcp` inode | `GetExtendedTcpTable(TCP_TABLE_OWNER_PID_LISTENER)` gives port to pid directly. Also `GetExtendedTcp6Table`. | 60 lines |
| `process.rs` stdio to log, rotation | `O_NOFOLLOW`, dup | `CreateFile` with `FILE_FLAG_OPEN_REPARSE_POINT` to refuse junctions, or skip since the dir is per user. | 30 lines |
| `docs_state.rs`, `settings_file.rs`, `scene.rs` | `O_NOFOLLOW` reads | `symlink_metadata` check before open, or `FILE_FLAG_OPEN_REPARSE_POINT`. Symlinks need admin on Windows so the risk is lower. | 20 lines |
| `file_uri.rs`, `root.rs` | `file:///abs/path` | Drive letters: `file:///C:/x`, case-insensitive compare, backslash to slash, UNC paths refused. Godot sends `res://` and `file:///C:/...` with the drive upper-cased. Match its form exactly or completion targets miss. | 80 lines, plus tests |
| `godot_bin.rs` | `godot4`, `godot` on PATH | `godot.exe`, `Godot_v4*.exe`, `%LOCALAPPDATA%\Programs\Godot`, Steam path. Godot's console variant `*_console.exe` is needed for stdout, the GUI exe detaches from the console. | 40 lines |
| `main.rs` | `mallopt` | remove under `cfg` | 3 lines |
| `run.rs` | inherits stdio, `waitpid` | works via `std::process`, but the headless flag needs `--headless` still and the display driver flags differ. Verify. | small |
| extension | `godot-bridge` on PATH | `worktree.which` finds `.exe` already. Tasks in `docs/sample` call `godot-bridge`, fine on PATH. | none |

## Design notes

- Path identity: the project hash today is FNV of the canonical path. On
  Windows canonicalize returns `\\?\C:\...`. Strip the prefix and lower-case
  before hashing so the same project always maps to one state file.
- Zed sends `file:///c%3A/...` on Windows. Decode, upper-case the drive,
  and compare with Godot's `file:///C:/...` form. One normalization function
  used by both sides.
- Process identity is pid plus creation time, same idea as start ticks.
- The `dap` subcommand finds the owner through the pipe name, same protocol
  and JSON as today.
- CRLF in scripts: the document text goes through unchanged, Godot handles
  it. Line endings in the settings file are handled by the JSON parser.
- Console window: the bridge is launched by Zed without a console. Godot
  headless writes to its stdout, so capture through pipes rather than
  inherited handles.

## Steps

1. `sys/windows.rs` with the same signatures as `sys/linux.rs`.
2. Paths and URIs first, with tests, since every other module depends on
   them.
3. Lock, runtime dir, state file, named pipe server and client.
4. Job object spawn, kill, and creation time identity.
5. `ReadDirectoryChangesW` watcher producing `WatcherChange`.
6. Port ownership through the TCP table.
7. Godot discovery and the console exe rule.
8. Full suite on Windows with Godot 4 installed. GUI hand-off and swap back
   with the job object rules. Debug session attach and launch.
9. `docs/dev-setup.md` for Windows, README platform line.

## Risks

- Godot on Windows binds LSP to `127.0.0.1` by default but a user setting can
  make it `0.0.0.0`. The port ownership check by pid handles both.
- Antivirus can hold `.gd` files open briefly after save; the watcher
  `Modified` may arrive before the file is readable. Retry once after
  50 ms, or treat a read failure as a later `Modified`.
- Long path support needs the manifest flag or `\\?\` paths everywhere.
  Use `\\?\` internally and strip for display and URIs.
