#!/usr/bin/env bash
# Grant or inspect SHA-scoped exceptions to merge-gate conditions.

set -euo pipefail
umask 077

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=mandate-lib.sh
source "$SCRIPT_DIR/mandate-lib.sh"

command -v jq >/dev/null 2>&1 || { echo "mandate excuse: jq is required" >&2; exit 1; }
command -v gh >/dev/null 2>&1 || { echo "mandate excuse: gh is required" >&2; exit 1; }

usage() {
  echo "usage: excuse.sh grant <owner/repo#number> <condition> <reason...> | list [<owner/repo#number>]" >&2
  exit 2
}

valid_target() {
  [[ "$1" =~ ^[^/[:space:]#]+/[^/[:space:]#]+#[1-9][0-9]*$ ]]
}

resolve_head() {
  resolved_target="$1"
  resolved_repo="${resolved_target%#*}"
  resolved_number="${resolved_target##*#}"
  resolved_file="$2"

  if ! gh pr view "$resolved_number" --repo "$resolved_repo" --json headRefOid \
    >"$resolved_file"; then
    echo "mandate excuse: could not resolve $resolved_target" >&2
    return 3
  fi
  resolved_head="$(jq -r '.headRefOid // empty' "$resolved_file" 2>/dev/null)"
  if [[ ! "$resolved_head" =~ ^[0-9a-fA-F]{40}$ ]]; then
    echo "mandate excuse: $resolved_target did not return a full 40-character head SHA" >&2
    return 3
  fi
  printf '%s\n' "$resolved_head"
}

action="${1:-list}"
case "$action" in
  grant)
    [ "$#" -ge 4 ] || usage
    target="$2"
    condition="$3"
    shift 3
    reason="$*"
    valid_target "$target" || usage
    case "$condition" in
      required_checks|review_threads|bounce_selectors|reserved_refs|merge_protocol) ;;
      *)
        echo "mandate excuse: condition must be one of required_checks, review_threads, bounce_selectors, reserved_refs, merge_protocol" >&2
        exit 2
        ;;
    esac
    reason="$(jq -rn --arg reason "$reason" \
      '$reason | gsub("^[[:space:]]+|[[:space:]]+$"; "")')"
    [ -n "$reason" ] || {
      echo "mandate excuse: reason must not be empty" >&2
      exit 2
    }

    work="$(mktemp -d "${TMPDIR:-/tmp}/mandate-excuse.XXXXXX")" || {
      echo "mandate excuse: could not create temporary workspace" >&2
      exit 3
    }
    trap 'rm -rf "$work"' EXIT
    head_sha="$(resolve_head "$target" "$work/pr.json")" || exit $?
    repo="${target%#*}"
    number="${target##*#}"
    record="$(
      jq -cn \
        --arg ts "${MANDATE_EXCUSE_TIME:-$(date -u +%Y-%m-%dT%H:%M:%SZ)}" \
        --arg repo "$repo" \
        --argjson pr "$number" \
        --arg head_sha "$head_sha" \
        --arg condition "$condition" \
        --arg reason "$reason" '
          {
            ts: $ts,
            repo: $repo,
            pr: $pr,
            head_sha: $head_sha,
            condition: $condition,
            reason: $reason
          }
        '
    )"
    mkdir -p "$MANDATE_DATA_DIR"
    printf '%s\n' "$record" >>"$MANDATE_EXCEPTIONS_LOG"
    printf '%s\n' "$record"
    ;;
  list)
    [ "$#" -le 2 ] || usage
    filter="${2:-}"
    [ -z "$filter" ] || valid_target "$filter" || usage
    [ -s "$MANDATE_EXCEPTIONS_LOG" ] || exit 0
    if ! jq -s -e '
      all(.[];
        type == "object"
        and (.ts | type == "string")
        and (.repo | type == "string")
        and (.pr | type == "number")
        and (.head_sha | type == "string")
        and (.condition | type == "string")
        and (.reason | type == "string")
      )
    ' "$MANDATE_EXCEPTIONS_LOG" >/dev/null; then
      echo "mandate excuse: cannot read $MANDATE_EXCEPTIONS_LOG" >&2
      exit 3
    fi

    work="$(mktemp -d "${TMPDIR:-/tmp}/mandate-excuse.XXXXXX")" || {
      echo "mandate excuse: could not create temporary workspace" >&2
      exit 3
    }
    trap 'rm -rf "$work"' EXIT
    current_heads='{}'
    while IFS= read -r current_target; do
      [ -n "$current_target" ] || continue
      # A pull request that no longer resolves must not take the whole listing
      # down with it. `list` reads an append-only audit log, and an audit log
      # that refuses to print because one old record points at a deleted pull
      # request is worse than one that prints the record and admits it cannot
      # classify it.
      current_head="$(resolve_head "$current_target" "$work/pr.json" 2>/dev/null)" ||
        current_head=""
      current_heads="$(jq -c \
        --arg target "$current_target" \
        --arg head "$current_head" \
        '. + {($target): $head}' <<<"$current_heads")"
    done < <(
      jq -sr --arg filter "$filter" '
        [.[]
          | (.repo + "#" + (.pr | tostring)) as $target
          | select($filter == "" or $target == $filter)
          | $target
        ]
        | unique[]
      ' "$MANDATE_EXCEPTIONS_LOG"
    )

    jq -sr --arg filter "$filter" --argjson heads "$current_heads" '
      .[]
      | (.repo + "#" + (.pr | tostring)) as $target
      | select($filter == "" or $target == $filter)
      | (if ($heads[$target] // "") == "" then "unknown"
         elif $heads[$target] == .head_sha then "current"
         else "superseded" end)
        + " " + $target
        + " " + .condition
        + " head_sha=" + .head_sha
        + " reason=" + (.reason | tojson)
    ' "$MANDATE_EXCEPTIONS_LOG"
    ;;
  *) usage ;;
esac
