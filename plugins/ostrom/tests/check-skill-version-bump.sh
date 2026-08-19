#!/usr/bin/env bash

set -Eeuo pipefail

base_ref="${1:-origin/main}"
head_ref="${2:-HEAD}"

if ! merge_base="$(git merge-base "$base_ref" "$head_ref")"; then
  echo "protocol version check: cannot find merge base for $base_ref and $head_ref" >&2
  exit 2
fi

version_at() {
  local ref="$1"
  local manifest="$2"

  if ! git cat-file -e "${ref}:${manifest}" 2>/dev/null; then
    printf '<missing>'
    return
  fi

  git show "${ref}:${manifest}" | node -e '
    let source = "";
    process.stdin.setEncoding("utf8");
    process.stdin.on("data", (chunk) => { source += chunk; });
    process.stdin.on("end", () => {
      const version = JSON.parse(source).version;
      if (typeof version !== "string" || version.length === 0) {
        throw new Error("plugin manifest has no non-empty string version");
      }
      process.stdout.write(version);
    });
  '
}

# Skills are not the only shipped surface. `scripts/`, `hooks/` and the built
# `dist/` bundle reach an installed session through the same versioned plugin
# cache, and that cache is keyed by version: an unchanged version means the
# cached tree is never refetched, so a fix to one of them silently never ships.
#
# ostrom#293 is the worked example. It fixed the defect that had the builder
# working zero items every hour, changed no SKILL.md, and so did not bump the
# version — leaving the repaired script in the repository and the broken one in
# every installed cache, permanently and with nothing reporting it.
mapfile -t changed_shipped < <(
  git diff --name-only --diff-filter=ACMRD "$merge_base" "$head_ref" -- \
    'plugins/*/skills/*/SKILL.md' \
    'plugins/*/scripts/*' \
    'plugins/*/hooks/*' \
    'plugins/*/runtime/*' \
    'plugins/*/dist/*' |
    LC_ALL=C sort
)

declare -A base_versions=()
declare -A head_versions=()
status=0

for shipped_path in "${changed_shipped[@]}"; do
  if [[ ! "$shipped_path" =~ ^plugins/([^/]+)/(skills|scripts|hooks|dist)/ ]]; then
    continue
  fi

  plugin="${BASH_REMATCH[1]}"
  manifest="plugins/$plugin/.claude-plugin/plugin.json"
  if [[ ! -v "base_versions[$plugin]" ]]; then
    base_versions["$plugin"]="$(version_at "$merge_base" "$manifest")"
    head_versions["$plugin"]="$(version_at "$head_ref" "$manifest")"
  fi

  if [[ "${base_versions[$plugin]}" == "${head_versions[$plugin]}" ]]; then
    printf '%s\n' \
      "protocol version check: plugin '$plugin' changed shipped file '$shipped_path' without changing version in $manifest (still ${head_versions[$plugin]}); the cache is keyed by version, so this change would never reach an installed session" \
      >&2
    status=1
  fi
done

exit "$status"
