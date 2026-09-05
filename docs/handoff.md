# Handoff

State as of 2026-09-05, after 20b2ae3.

## Done

- Full implementation per SPEC.md v11, all 22 TASKS.md items ticked.
  Verified on DieQuest at 8495a58: LSP start, crash recovery, DAP
  breakpoint hit, GUI hand-off and swap back.
- Extension depends on `zed_extension_api` only (73983e6).
- Dependency elimination per `docs/deps-plan.md`, all 12 steps. The bridge
  depends on `libc` only; `serde_json` stays as a dev dependency for the
  differential JSON parser test.
- Performance pass and eight review rounds (simplicity, performance,
  security), then a final codex review. All said ship. Details in
  `docs/review-log.md`, round 13 onward.

## Measured at b6eeaac (release, 1056 file project)

- Load: 1056 diagnostics in 3.3 s, bridge CPU 50 ms.
- RSS: 3.7 MB after initialize, 8.3 MB after load and symbol fill.
- didChange of 52 KB: about 80 us main thread CPU. Small frames: about 1 us.
- Symbol search round trip: 0.26 ms for "ready", 0.43 ms for "" on 21.8k
  symbols.
- Idle: 0 wakeups on all 7 threads.

## Checks

```
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test -p godot-bridge
```

All green at b6eeaac: 98 unit tests, 6 integration binaries. Five
concurrent full-suite runs pass.

## Supply chain gates (2026-09-05)

- `deny.toml`: yanked deny, wildcards deny, multiple versions warn, license
  allowlist, crates.io only. Own crates are `publish = false` and skipped
  by the license check because the repo has no license yet.
- `supply-chain/`: cargo vet with six imports (bytecode-alliance, embark,
  google, isrg, mozilla, zcash). Baseline: 26 audited, 1 partial,
  61 exempted of 88 crates. New crates fail `cargo vet` until audited or
  exempted on purpose.
- `scripts/check_build_scripts.py`: fails when a crate with a build script
  or proc macro is missing from `scripts/build_scripts.allow` (24 today).
- CI: `cargo fetch --locked` then every cargo step `--locked --offline`,
  cargo-deny, cargo-audit, cargo-vet on each push, cargo-geiger weekly
  (informational, its output is not asserted). Actions pinned to commit
  SHAs, Dependabot bumps them. Token is read-only except the audit report.
- The build-script allowlist is name only. A version bump of an allowed
  crate is caught by cargo vet, which pins exact versions.
- Local run: `cargo deny check`, `cargo audit`, `cargo vet`,
  `python3 scripts/check_build_scripts.py`.

## Next, in order

1. Restart Zed and re-verify on DieQuest with the reinstalled bridge and
   the rebuilt extension wasm (both from HEAD, 2026-09-05).
2. Pick a license and add `LICENSE`, then drop `private.ignore` in
   `deny.toml`.
3. Deferred perf item, opt-in only: the didChange rewrite re-escapes the
   text (about 20 us of the 80 us). Not worth the code at typing rates.

## Machine setup

- `~/.local/share/zed/extensions/installed/godot` symlinks to `extension/`.
  Zed rescans only after deleting `~/.local/share/zed/extensions/index.json`.
- `~/.local/bin/godot-bridge` symlinks to `~/.cargo/bin/godot-bridge`.
  Reinstall with `cargo install --path bridge --locked`.
- Grammar wasm files are built by hand with wasi-sdk 34, see
  `docs/dev-setup.md`. They are gitignored.
- Kill Zed with `pkill -x zed-editor`. `pkill -f` matches this repo path.

## Agents

- codex profiles (`run-opencode`) work but the wrapper sometimes dies
  mid-run (exit 143) after the work is staged or committed. Check
  `git status` and `git log` before rerouting. Fallback: Claude agents with
  these rules in every prompt: zero comments, smallest code that passes,
  no speculative abstractions, plain names over comments.
- Codex shell policy blocks `git commit`; it stages, you commit.
- Concurrent in-place agents on the same crate see each other's half edits.
  Run bridge work serially or in worktrees.
