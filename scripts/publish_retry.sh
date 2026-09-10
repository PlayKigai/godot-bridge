#!/usr/bin/env sh
# Usage: publish_retry.sh <vsce|ovsx> <vsix>
# Both registries time out under load often enough that one attempt is not enough,
# and a timed-out upload sometimes still lands, so an existing version counts as success.
set -eu
tool=$1
vsix=$2
attempt=1
while [ "$attempt" -le 3 ]; do
  case $tool in
    vsce) out=$(./node_modules/.bin/vsce publish --packagePath "$vsix" 2>&1) && { echo "$out"; exit 0; } ;;
    ovsx) out=$(./node_modules/.bin/ovsx publish "$vsix" 2>&1) && { echo "$out"; exit 0; } ;;
    *) echo "unknown tool $tool" >&2; exit 2 ;;
  esac
  echo "$out"
  if echo "$out" | grep -qiE "already exists|already published|same version"; then
    echo "$tool: version already published, nothing to do"
    exit 0
  fi
  echo "$tool: attempt $attempt failed, retrying"
  attempt=$((attempt + 1))
  sleep 30
done
echo "$tool: giving up after 3 attempts" >&2
exit 1
