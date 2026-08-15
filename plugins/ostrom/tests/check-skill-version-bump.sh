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

mapfile -t changed_skills < <(
  git diff --name-only --diff-filter=ACMRD "$merge_base" "$head_ref" -- \
    'plugins/*/skills/*/SKILL.md' |
    LC_ALL=C sort
)

declare -A base_versions=()
declare -A head_versions=()
status=0

for skill_path in "${changed_skills[@]}"; do
  if [[ ! "$skill_path" =~ ^plugins/([^/]+)/skills/[^/]+/SKILL\.md$ ]]; then
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
      "protocol version check: plugin '$plugin' changed skill file '$skill_path' without changing version in $manifest (still ${head_versions[$plugin]})" \
      >&2
    status=1
  fi
done

exit "$status"
