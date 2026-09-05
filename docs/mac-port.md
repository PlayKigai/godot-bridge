# macOS port

Estimate: two to three days for one person with a Mac, plus a day of
verification against a real project. No new crates beyond `libc`.

## What already works

Everything not listed below is POSIX and compiles unchanged: Unix owner
socket, `flock`, 0600 and 0700 modes, `O_NOFOLLOW`, `setsid`, `setpgid`,
`kill(-pgid)`, `poll`, pipes, `waitpid`, TCP proxying, the JSON parser, the
LSP and DAP logic. The extension is wasm and already cross-platform.

## Linux-only pieces

| module | Linux mechanism | macOS replacement | size |
|---|---|---|---|
| `watch.rs` | inotify | kqueue with `EVFILT_VNODE` on each directory fd, or FSEvents | largest, about 250 lines |
| `state.rs` `process_start_ticks` | `/proc/<pid>/stat` field 22 | `sysctl(KERN_PROC, KERN_PROC_PID)` and `kp_proc.p_starttime` | 30 lines |
| `process.rs` `port_listener_belongs_to_process` | `/proc/net/tcp` inode against `/proc/<pid>/fd` | `proc_pidinfo(PROC_PIDLISTFDS)` then `PROC_PIDFDSOCKETINFO` for the local port, from libproc | 60 lines |
| `process.rs` spawn | `prctl(PR_SET_PDEATHSIG)` so Godot dies with the bridge | no equivalent. Use the existing stale cleanup path on next start, or a watchdog pipe: pass the child a pipe fd and have it exit on EOF. Godot cannot do that, so accept orphan cleanup at next start. | 10 lines, mostly removal |
| `process.rs` grandchild check | `/proc/<pid>` exists | `kill(pid, 0)` with `ESRCH` | 5 lines |
| `main.rs` | `mallopt(M_ARENA_MAX)` | not available, remove under `cfg` | 3 lines |
| `state.rs` `runtime_dir` | `$XDG_RUNTIME_DIR` | `$TMPDIR` is per user and private on macOS; keep the uid check | 5 lines |
| `godot_bin.rs` | `godot4`, `godot` on PATH | also look in `/Applications/Godot.app/Contents/MacOS/Godot` | 10 lines |

## Watcher design

kqueue watches file descriptors, not paths. One open fd per directory, so a
1000 directory project holds 1000 fds. Raise the soft `RLIMIT_NOFILE` at start
and fall back to a coarse rescan when `EMFILE` hits, the same way inotify
`ENOSPC` is handled today. Events say only that a directory changed, not
which file, so on each event re-list that directory and diff against the
last listing to produce `Created`, `Modified`, `Removed`. Keep the current
`WatcherChange` enum and the single `Rescan` on overflow so `lsp/mod.rs` does
not change.

FSEvents avoids the fd limit and gives file paths, but needs a CFRunLoop
thread and Core Foundation bindings written by hand. kqueue is smaller.

## Steps

1. Move the Linux-only functions into `sys/linux.rs`, add `sys/macos.rs`
   with the same signatures, select with `cfg(target_os)`. Keep the shared
   code in place.
2. kqueue watcher with directory diffing. Reuse the existing watcher unit
   tests, they are path based.
3. `sysctl` start time and libproc port ownership. Unit test against the
   bridge's own pid and a listener it opens.
4. Remove PDEATHSIG and `mallopt` under `cfg`.
5. Godot binary discovery in `/Applications`.
6. Run the full suite on a Mac with Godot 4 installed. The integration tests
   spawn real Godot. Check GUI hand-off with the `.app` bundle: it must be
   launched as the inner binary, not with `open`, so the pid and ports are
   the child's.
7. Update `docs/dev-setup.md` and the README platform line.

## Risks

- Godot's macOS build may bind LSP on IPv6 first. The port ownership check
  must look at both families, as it does on Linux.
- kqueue delivers no event for a file written inside a directory that was
  moved in after start until that directory is opened and watched. The
  `Rescan` on directory create or move covers this.
- Debug sessions use the same ports and protocol, so DAP needs no change.
