#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
mkdir -p "$root/target"
fixture="$(mktemp -d "$root/target/ostrom-npm-pack-test.XXXXXX")"
trap 'rm -rf "$fixture"' EXIT

node --test "$root/npm/tests/distribution.test.mjs"
node "$root/npm/scripts/create-test-artifacts.mjs" --output "$fixture/artifacts"
node "$root/npm/scripts/stage-packages.mjs" \
  --artifacts "$fixture/artifacts" \
  --output "$fixture/packages"
bash "$root/npm/scripts/pack.sh" \
  --staging "$fixture/packages" \
  --output "$fixture/tarballs"
