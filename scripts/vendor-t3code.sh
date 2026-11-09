#!/usr/bin/env bash
# Copy the upstream T3 Code files the agreement test reads, verbatim, at the commit a release
# tag points to. Pin the tag of the installed `t3` binary (`t3 --version`), not a branch tip.
# Usage: scripts/vendor-t3code.sh <release-tag, e.g. v0.1.7>
# Afterwards, set the same tag and commit in src/t3/pin.ts and let CI run the agreement test.
set -euo pipefail

tag="${1:?usage: scripts/vendor-t3code.sh <release-tag>}"
repo="pingdotgg/t3code"

# An annotated tag lists its commit as <tag>^{}; a lightweight tag lists it directly.
refs="$(git ls-remote "https://github.com/$repo.git" "refs/tags/$tag" "refs/tags/$tag^{}")"
rev="$(awk -v peeled="refs/tags/$tag^{}" '$2 == peeled { print $1 }' <<<"$refs")"
[[ -n "$rev" ]] || rev="$(awk -v ref="refs/tags/$tag" '$2 == ref { print $1 }' <<<"$refs")"
[[ "$rev" =~ ^[0-9a-f]{40}$ ]] || { echo "tag $tag not found in $repo" >&2; exit 1; }

root="$(cd "$(dirname "$0")/.." && pwd)"
dest="$root/vendor/t3code"

# The import closure of packages/contracts/src/orchestratorMcp.ts, plus the CLI command file
# that prints `t3 thread ... --json`. The CLI file is read as text only; it is never imported.
files=(
  LICENSE
  apps/server/src/cli/thread.ts
)
for name in auth baseSchemas chatAttachment checkpointDiff composerContext environment guestShare \
  model modelSelection orchestrationProject orchestrationV2 orchestrationV2IdTable orchestratorMcp \
  project providerInstance providerPolicy providerRuntime pullRequest scheduledTask secretReference \
  sourceControl threadMetadataMcp threadPullRequest threadSearch threadTags \
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
  echo "- Tag: $tag"
  echo "- Commit: $rev"
  echo "- Tree: https://github.com/$repo/tree/$rev"
  echo
  echo "| File | sha256 |"
  echo "|---|---|"
  for path in "${files[@]}"; do
    echo "| \`$path\` | \`$(sha256sum "$dest/$path" | cut -d' ' -f1)\` |"
  done
} > "$dest/SOURCE.md"
