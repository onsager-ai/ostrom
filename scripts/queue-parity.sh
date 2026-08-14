#!/usr/bin/env bash
# Compare the Rust and Bash queue readers over caller-supplied runtime state.
# No queue is captured here: private state remains on the operator's machine.

set -euo pipefail

[ -n "${OSTROM_HOME:-}" ] || {
  echo "queue parity: OSTROM_HOME must point at an Ostrom state directory" >&2
  exit 2
}
[ -d "$OSTROM_HOME" ] || {
  echo "queue parity: OSTROM_HOME is not a directory: $OSTROM_HOME" >&2
  exit 2
}

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
ln -s "$OSTROM_HOME" "$work/ostrom"

CLAUDE_CONFIG_DIR="$work" CLAUDE_PLUGIN_ROOT="$repo_root/plugins/ostrom" \
  bash "$repo_root/plugins/ostrom/scripts/queue.sh" list >"$work/bash.jsonl"
cargo run --quiet --manifest-path "$repo_root/Cargo.toml" -p ostrom-cli -- \
  queue list --format=json >"$work/rust.jsonl"

cmp "$work/bash.jsonl" "$work/rust.jsonl"
echo "queue parity: byte-identical"
