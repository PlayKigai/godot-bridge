#!/usr/bin/env sh
# Usage: check_release_version.sh vX.Y.Z  (fails unless every client and the workspace carry X.Y.Z)
set -eu
tag=$1
case $tag in
  v[0-9]*.[0-9]*.[0-9]*) ;;
  *) echo "tag $tag is not vX.Y.Z" >&2; exit 1 ;;
esac
want=${tag#v}
echo "$want" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$' || { echo "tag $tag is not strict semver" >&2; exit 1; }
cargo_v=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
zed_v=$(sed -n 's/^version = "\(.*\)"/\1/p' clients/zed/extension.toml | head -1)
lua_v=$(sed -n 's/^local VERSION = "\(.*\)"/\1/p' lua/godot-bridge/init.lua | head -1)
npm_v=$(sed -n 's/^  "version": "\(.*\)",/\1/p' clients/vscode/package.json | head -1)
status=0
for pair in "Cargo.toml=$cargo_v" "clients/zed/extension.toml=$zed_v" "lua/godot-bridge/init.lua=$lua_v" "clients/vscode/package.json=$npm_v"; do
  file=${pair%%=*}; got=${pair#*=}
  if [ "$got" != "$want" ]; then echo "$file has version '$got', tag wants '$want'" >&2; status=1; fi
done
exit $status
