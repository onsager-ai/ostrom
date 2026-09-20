#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
mkdir -p "$root/target"
fixture="$(mktemp -d "$root/target/ostrom-npm-pack-test.XXXXXX")"
trap 'rm -rf "$fixture"' EXIT

node --test "$root/npm/tests/distribution.test.mjs" "$root/npm/tests/publish-idempotent.test.mjs"
node "$root/npm/scripts/create-test-artifacts.mjs" --output "$fixture/artifacts"
node "$root/npm/scripts/stage-packages.mjs" \
  --artifacts "$fixture/artifacts" \
  --output "$fixture/packages"
bash "$root/npm/scripts/pack.sh" \
  --staging "$fixture/packages" \
  --output "$fixture/tarballs"

# publish.mjs --dry-run is deliberately not exercised here: `npm publish
# --dry-run` still performs a login/auth check against the configured
# registry even with bytes fully local (measured on this machine — it hangs
# past a short timeout against an unreachable registry rather than failing
# closed), so a dry-run publish step cannot be made to run offline.
