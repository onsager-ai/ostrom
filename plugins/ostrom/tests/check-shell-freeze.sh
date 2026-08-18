#!/usr/bin/env bash

set -Eeuo pipefail

base_ref="${1:-origin/main}"
head_ref="${2:-HEAD}"

if ! merge_base="$(git merge-base "$base_ref" "$head_ref")"; then
  echo "shell freeze: cannot find merge base for $base_ref and $head_ref" >&2
  exit 2
fi

line_count_at() {
  local ref="$1"
  local path="$2"
  local count

  if ! git cat-file -e "${ref}:${path}" 2>/dev/null; then
    printf '0\n'
    return
  fi

  if ! count="$(git show "${ref}:${path}" | awk 'END { print NR }')"; then
    printf '%s\n' "shell freeze: cannot count lines in $path at $ref" >&2
    exit 2
  fi
  printf '%s\n' "$count"
}

if ! changed_output="$(
  git diff --name-only --diff-filter=ACMRD "$merge_base" "$head_ref" -- \
    'plugins/ostrom/scripts/' |
    LC_ALL=C sort
)"; then
  echo "shell freeze: cannot inspect changes between $merge_base and $head_ref" >&2
  exit 2
fi

changed_scripts=()
if [[ -n "$changed_output" ]]; then
  mapfile -t changed_scripts <<<"$changed_output"
fi

# Scripts that are permanently exempt from the retirement, and therefore from
# the freeze. `run-node.sh` resolves the operator's Node runtime across PATH,
# nvm, fnm, volta and asdf, because plugin scripts run non-interactively and
# cannot source an interactive profile. Rust does not dissolve that problem —
# the `doctor` bundle is Node, so something must still find Node, and porting
# 119 lines of shell path-searching to 119 lines of Rust path-searching moves
# it rather than retiring it. Confirmed the hard way on 2026-08-18, when the
# operator's systemd units could resolve neither `ostrom` nor `node` from a
# non-interactive login shell and the same search had to be written again.
#
# An exempt script is a living file, not a frozen one: it stays, so it must be
# allowed to be maintained. Freezing a file nobody may delete only guarantees
# it rots. Decided by the principal on 2026-08-18 (see #275).
exempt_scripts=(
  "plugins/ostrom/scripts/run-node.sh"
)

is_exempt() {
  local candidate="$1"
  local exempt
  for exempt in "${exempt_scripts[@]}"; do
    if [[ "$candidate" == "$exempt" ]]; then
      return 0
    fi
  done
  return 1
}

has_bugfix_label=false
while IFS= read -r label; do
  if [[ "$label" == "bash-bugfix" ]]; then
    has_bugfix_label=true
    break
  fi
done <<<"${PULL_REQUEST_LABELS:-}"

status=0
for script_path in "${changed_scripts[@]}"; do
  base_lines="$(line_count_at "$merge_base" "$script_path")"
  head_lines="$(line_count_at "$head_ref" "$script_path")"
  delta=$((head_lines - base_lines))

  if ((delta <= 0)); then
    continue
  fi

  if is_exempt "$script_path"; then
    printf '%s\n' \
      "shell freeze: $script_path grew by $delta lines ($base_lines -> $head_lines); permanently exempt from the retirement." \
      >&2
    continue
  fi

  if [[ "$has_bugfix_label" == true ]]; then
    printf '%s\n' \
      "shell freeze: $script_path grew by $delta lines ($base_lines -> $head_lines); permitted by the bash-bugfix label." \
      >&2
    continue
  fi

  printf '%s\n' \
    "shell freeze: $script_path grew by $delta lines ($base_lines -> $head_lines); implement this in Rust, or label the pull request bash-bugfix if it is a defect fix." \
    >&2
  status=1
done

exit "$status"
