# Dependency plan

Rule: a dependency that this tool can replace with simple code stays out.
Target: the bridge depends on `libc` only. The extension depends on
`zed_extension_api` only (it is the ABI).

Two independent analyses (2026-09-04) reached the same table.

| crate | decision | replacement | lines |
|---|---|---|---|
| nix | delete | unused | 0 |
| libc | keep | bindings only, no logic; `prctl` is variadic and per-arch constants are a liability by hand | 0 |
| anyhow | replace | `error.rs`: `Error(String)`, `Result<T>`, `From<io::Error>`, `.context()`, `bail!` | 60 |
| tracing, tracing-subscriber | replace | `log.rs`: level from `GODOT_BRIDGE_LOG`, `error!/warn!/info!/debug!` to stderr | 50 |
| time | replace | `clock.rs`: UTC rfc3339 from `SystemTime` via civil_from_days | 45 |
| blake3 | replace | `fnv.rs`: FNV-1a 64 as 16 hex chars, names runtime files only | 12 |
| clap | replace | `cli.rs`: 7 subcommands, `--file`, `--scene`, `--file=x` form, `--` trailing ignored, hand-written help | 140 |
| url, percent-encoding | replace | `file_uri.rs`: encode per segment matching `url::Url::from_file_path`, decode to bytes | 130 |
| serde, serde_json, json5 | replace | `json/`: `Value` with insertion-ordered object and lexical numbers, strict parser, JSONC relaxed mode, writer, `json!` macro, manual `Settings`/`State` codec | 700 |
| notify | replace | `watch.rs`: raw inotify, recursive watches with the existing skip list, overflow triggers a rescan | 300 |
| tokio | replace | std threads: one reader thread per stream feeding one owner thread over `mpsc`, absolute deadlines with `recv_timeout` | net +150 |
| tempfile (dev) | replace | `tests/support` TempDir | 40 |
| serde_json (dev only) | keep | differential test of the JSON parser over a corpus and the JSONTestSuite cases | 0 shipped |

Total added: about 1,600 lines of code plus 500 of tests.

## Hard rules for the port

- JSON numbers keep their lexical form. `id.to_string()` is a hash key for
  request tracking; a reformatted number breaks stale-id matching silently.
- String unescape copies runs, never single chars. Depth limit 128.
- `Index`/`IndexMut` on `Value` auto-create objects, like serde_json, so call
  sites stay unchanged.
- `path_to_uri` must equal `url::Url::from_file_path` byte for byte. Pin a test
  against a URI captured from a live Zed session.
- inotify: `IN_CLOSE_WRITE` and `IN_MOVED_TO` cover saves. `IN_Q_OVERFLOW`
  emits a resync change that reruns the project scan. `ENOSPC` on add_watch
  logs once and continues partially watched.
- Threads: frame-bearing events use `sync_channel` with capacity 1 or 2 so a
  slow Godot backs up into Zed's pipe as today. Cheap events use an unbounded
  channel. Every connection swap does `shutdown(SHUT_RDWR)`, joins the reader
  thread, then drops the socket. Reader thread stacks 256 KiB.
- Timers are stored `Instant`s, never recreated per loop iteration. This also
  fixes the current symbol-sweep and GUI-liveness starvation under sustained
  traffic.

## Order

Each step ships alone with the integration suite green.

1. delete nix
2. tracing to log.rs
3. time to clock.rs
4. blake3 to fnv.rs, delete dead `root::project_hash`
5. anyhow to error.rs
6. clap to cli.rs
7. split bridge into lib.rs plus a thin main.rs so tests import bridge modules
8. url and percent-encoding to file_uri.rs
9. serde, serde_json, json5 to json/ with the differential dev test
10. notify to watch.rs
11. tokio to threads
12. tempfile to tests/support

## Known bugs in the current code that the port must fix

- `shutdown` handling reads one Godot frame and treats it as the response; a
  notification arriving first is mistaken for it.
- Watcher overflow is logged and dropped with no rescan.
- `empty_state` is a dead alias for `new_state`.
