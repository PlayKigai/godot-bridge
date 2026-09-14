# Releasing

Versions are immutable. The `v*` ruleset forbids updating or deleting a tag
and has no bypass, so a tag you regret is spent: fix the problem and release
the next patch version. Editing the ruleset is the only escape hatch and is
not worth using.

## Cutting a release

1. Bump all four version strings to the same value: `Cargo.toml`
   (`[workspace.package]`), `clients/zed/extension.toml`,
   `lua/godot-bridge/init.lua`, `clients/vscode/package.json`. Run
   `cargo check` so `Cargo.lock` follows.
2. Verify before tagging, because the tag is what costs a version:
   `sh scripts/check_release_version.sh vX.Y.Z`.
3. Push `main`, then push the tag. `release.yml` runs the supply-chain gate,
   builds Linux and Windows, packages the VSIX, attests every artifact,
   writes `SHA256SUMS`, and waits for your approval before creating the
   GitHub release and publishing to crates.io.
4. Publish the extension separately once the release exists:
   `gh workflow run publish.yml --ref main -f tag=vX.Y.Z`, then approve.

## When something fails

**Any job before the release job.** Nothing is public. Re-run failed jobs if
the cause was transient. If the cause was the code or a version mismatch, the
tag is spent; fix and cut the next patch version.

**The release job fails partway.** Some assets may be attached. Re-running is
safe: the job uploads with `--clobber` and creates the release only if it does
not already exist.

**crates.io fails after the release exists.** The GitHub release stands and
`cargo install godot-bridge` keeps serving the previous version. Re-run the
`crates-io` job. It probes crates.io first and does nothing if the version
already landed.

**A publish times out.** The Marketplace returns `Request timeout:
/_apis/gallery` after three minutes even when the upload may have succeeded.
A timeout is not proof of failure. Check the registry page, then re-dispatch.
Both publishers run with `--skip-duplicate`, so an already-published version
is a no-op. There is deliberately no automatic retry.

**One registry lands and the other does not.** They are separate jobs on
purpose, so one can never block the other. Re-dispatch; the one that already
published skips.
