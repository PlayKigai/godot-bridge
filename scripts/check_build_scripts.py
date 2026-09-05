#!/usr/bin/env python3
"""Fail when a crate with a build script or proc macro is not in scripts/build_scripts.allow."""
import json
import pathlib
import subprocess
import sys

allow_file = pathlib.Path(__file__).with_name("build_scripts.allow")
allowed = {line.strip() for line in allow_file.read_text().splitlines() if line.strip() and not line.startswith("#")}
metadata = json.loads(subprocess.check_output(["cargo", "metadata", "--format-version", "1", "--locked", "--all-features"]))
found = set()
for package in metadata["packages"]:
    kinds = {kind for target in package["targets"] for kind in target["kind"]}
    if kinds & {"custom-build", "proc-macro"}:
        found.add(package["name"])
unexpected = sorted(found - allowed)
stale = sorted(allowed - found)
if stale:
    print("allowlist entries no longer in the tree:", ", ".join(stale))
if unexpected:
    print("crates with build scripts or proc macros not in the allowlist:", ", ".join(unexpected))
    sys.exit(1)
