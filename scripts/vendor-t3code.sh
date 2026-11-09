#!/usr/bin/env bash
# Copy the upstream T3 Code files the agreement test reads, verbatim, at one commit.
# Usage: scripts/vendor-t3code.sh <full-commit-sha>
# Afterwards, set the same commit in src/t3/pin.ts and let CI run the agreement test.
set -euo pipefail

rev="${1:?usage: scripts/vendor-t3code.sh <full-commit-sha>}"
[[ "$rev" =~ ^[0-9a-f]{40}$ ]] || { echo "expected a full 40-character commit sha" >&2; exit 1; }

repo="pingdotgg/t3code"
root="$(cd "$(dirname "$0")/.." && pwd)"
dest="$root/vendor/t3code"

# The import closure of packages/contracts/src/orchestratorMcp.ts, plus the CLI command file
# that prints `t3 thread ... --json`. The CLI file is read as text only; it is never imported.
files=(
  LICENSE
  apps/server/src/cli/thread.ts
  packages/contracts/package.json
)
for name in auth baseSchemas chatAttachment checkpointDiff composerContext environment guestShare \
  model modelSelection orchestrationProject orchestrationV2 orchestrationV2IdTable orchestratorMcp \
  project providerInstance providerPolicy providerRuntime pullRequest scheduledTask secretReference \
  secretRequest sourceControl threadMetadataMcp threadPullRequest threadSearch threadTags \
  threadTitle vcs; do
  files+=("packages/contracts/src/$name.ts")
done

rm -rf "$dest"
for path in "${files[@]}"; do
  mkdir -p "$dest/$(dirname "$path")"
  curl -fsSL "https://raw.githubusercontent.com/$repo/$rev/$path" -o "$dest/$path"
done

{
  echo "# Vendored from T3 Code"
  echo
  echo "Copied verbatim by \`scripts/vendor-t3code.sh\`. Do not edit; re-run the script to re-pin."
  echo
  echo "- Repository: https://github.com/$repo"
  echo "- Commit: $rev"
  echo "- Tree: https://github.com/$repo/tree/$rev"
  echo
  echo "| File | sha256 |"
  echo "|---|---|"
  for path in "${files[@]}"; do
    echo "| \`$path\` | \`$(sha256sum "$dest/$path" | cut -d' ' -f1)\` |"
  done
} > "$dest/SOURCE.md"
