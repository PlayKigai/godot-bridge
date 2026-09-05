# Handoff

State as of 2026-09-04, commit 8495a58.

## Done

- Full implementation per SPEC.md v11, all 22 TASKS.md items ticked.
  Verified on DieQuest: LSP start, crash recovery, DAP breakpoint hit,
  1056 diagnostics in 7 s, symbol search 21 ms, GUI hand-off and swap back.
- Review round 1 (simplicity, performance, security) fixed in the bridge.
  Agent report with per-item outcomes is summarized in the commit message.
- Extension fix round done (73983e6). Extension depends on
  `zed_extension_api` only.
- Dependency elimination plan agreed by two independent analyses:
  `docs/deps-plan.md`. Not started.

## Checks

```
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test -p godot-bridge
```

All green at 8495a58: 56 unit tests, 6 integration binaries.

## Next, in order

1. Dependency elimination per `docs/deps-plan.md`, steps 1 to 12, tokio last.
   One implementer at a time, the whole bridge is one ownership unit.
   Each step: suite green, commit.
2. Supply chain gates: `deny.toml` (yanked deny, wildcards deny,
   multiple-versions warn, license allowlist, crates.io only),
   `cargo vet init` with imports, CI running cargo-deny, cargo-audit,
   weekly cargo-geiger, build-script and proc-macro allowlist via
   `cargo metadata`, `cargo build --locked --offline`.
   After step 1 the tree is `libc` only, so most of this is a guard against
   future additions.
3. Review round 2: simplicity, performance, security lenses again until all
   three say ship. Then reinstall the bridge, rebuild the extension wasm,
   restart Zed, re-verify on DieQuest.

## Machine setup

- `~/.local/share/zed/extensions/installed/godot` symlinks to `extension/`.
  Zed rescans only after deleting `~/.local/share/zed/extensions/index.json`.
- `~/.local/bin/godot-bridge` symlinks to `~/.cargo/bin/godot-bridge`.
  Reinstall with `cargo install --path bridge --locked`.
- Grammar wasm files are built by hand with wasi-sdk 34, see
  `docs/dev-setup.md`. They are gitignored.
- Kill Zed with `pkill -x zed-editor`. `pkill -f` matches this repo path.

## Agents

- codex profiles (`run-opencode`) were rate limited all day. Retry first.
  If still limited, use Claude Opus agents with these rules in every prompt:
  zero comments, smallest code that passes, no speculative abstractions,
  plain names over comments.
- Concurrent in-place agents on the same crate see each other's half edits.
  Run bridge work serially or in worktrees.
