#!/usr/bin/env bash
set -euo pipefail

base_sha="${1:-}"
if [[ -z "$base_sha" ]]; then
  echo "usage: $0 <pull-request-base-sha>" >&2
  exit 2
fi
if ! git cat-file -e "${base_sha}^{commit}" 2>/dev/null; then
  echo "plugin version gate: base commit $base_sha is not available" >&2
  exit 2
fi

declare -A changed_by_plugin=()
while IFS= read -r changed_file; do
  if [[ "$changed_file" =~ ^plugins/([^/]+)/.+$ ]]; then
    plugin="${BASH_REMATCH[1]}"
    manifest="plugins/$plugin/.claude-plugin/plugin.json"
    # A removed plugin ships no payload. New plugins and changed plugins do.
    if git cat-file -e "HEAD:$manifest" 2>/dev/null; then
      changed_by_plugin["$plugin"]="${changed_by_plugin[$plugin]:-$changed_file}"
    fi
  fi
done < <(git diff --name-only --diff-filter=ACMRTD "$base_sha"...HEAD -- plugins)

if (( ${#changed_by_plugin[@]} == 0 )); then
  echo "plugin version gate: no packaged plugin payload changed"
  exit 0
fi

version_at() {
  local revision="$1"
  local manifest="$2"
  git show "$revision:$manifest" 2>/dev/null | node -e '
    let source = "";
    process.stdin.setEncoding("utf8");
    process.stdin.on("data", (chunk) => { source += chunk; });
    process.stdin.on("end", () => {
      try {
        const value = JSON.parse(source).version;
        if (typeof value !== "string" || value.length === 0) process.exit(1);
        process.stdout.write(value);
      } catch {
        process.exit(1);
      }
    });
  '
}

failed=0
while IFS= read -r plugin; do
  manifest="plugins/$plugin/.claude-plugin/plugin.json"
  changed_file="${changed_by_plugin[$plugin]}"

  if ! head_version="$(version_at HEAD "$manifest")"; then
    echo "::error file=$manifest::Plugin '$plugin' payload changed at '$changed_file', but $manifest has no readable version."
    failed=1
    continue
  fi

  if git cat-file -e "$base_sha:$manifest" 2>/dev/null; then
    if ! base_version="$(version_at "$base_sha" "$manifest")"; then
      echo "::error file=$manifest::Plugin '$plugin' payload changed at '$changed_file', but the base version in $manifest is unreadable."
      failed=1
      continue
    fi
    if [[ "$head_version" == "$base_version" ]]; then
      echo "::error file=$changed_file::Plugin '$plugin' packaged payload changed at '$changed_file', but $manifest is still version $head_version. Bump the plugin version."
      failed=1
      continue
    fi
  fi

  echo "plugin version gate: $plugin payload changed at $changed_file; version is $head_version"
done < <(printf '%s\n' "${!changed_by_plugin[@]}" | sort)

exit "$failed"
