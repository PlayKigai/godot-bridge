# macOS port

No new crates. Everything POSIX compiles unchanged: Unix socket, `flock`,
file modes, `O_NOFOLLOW`, `setsid`, `setpgid`, `kill(-pgid)`, `poll`, pipes,
TCP proxying, JSON, LSP and DAP logic, the wasm extension.

| module | Linux | macOS | size |
|---|---|---|---|
| `watch.rs` | inotify | kqueue `EVFILT_VNODE` per directory fd, re-list and diff on event. Raise `RLIMIT_NOFILE`, fall back to `Rescan` on `EMFILE`. | 250 lines |
| `state.rs` start ticks | `/proc/<pid>/stat` | `sysctl(KERN_PROC_PID)` `p_starttime` | 30 |
| `process.rs` port owner | `/proc/net/tcp` | libproc `PROC_PIDLISTFDS` + `PROC_PIDFDSOCKETINFO`, both address families | 60 |
| `process.rs` spawn | `PR_SET_PDEATHSIG` | none. Rely on stale cleanup at next start. | 10 |
| `process.rs` liveness | `/proc/<pid>` | `kill(pid, 0)` | 5 |
| `main.rs` | `mallopt` | remove under `cfg` | 3 |
| `state.rs` runtime dir | `$XDG_RUNTIME_DIR` | `$TMPDIR`, keep the uid check | 5 |
| `godot_bin.rs` | PATH | also `/Applications/Godot.app/Contents/MacOS/Godot`. Launch the inner binary, not `open`, so pid and ports are the child's. | 10 |

Steps: `bridge/src/sys/` already splits the platforms; the Linux specifics
sit in `sys/unix/`, so split those out into a `sys/macos/` with the same
signatures, then the table top to bottom. Run the full suite with Godot 4
installed, including GUI hand-off.
