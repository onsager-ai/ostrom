#!/usr/bin/env bash
# SessionStart hook: render the last durable sweep as an exception digest.
#
# A machine with no private mandates.yaml emits nothing and exits 0. Once
# configured, this hook reads local files only and never makes a network call.

set -u

PLUGIN_ROOT="${CLAUDE_PLUGIN_ROOT:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
# shellcheck source=../scripts/mandate-lib.sh
source "$PLUGIN_ROOT/scripts/mandate-lib.sh"

mandate_is_configured || exit 0
command -v jq >/dev/null 2>&1 || {
  echo "mandate digest: jq is required" >&2
  exit 0
}

config="$(mandate_load_config)" || exit 0
cadence_hours="$(jq -r '.cadence_hours' <<<"$config")"

state_mtime=0
if [ -f "$MANDATE_STATE_FILE" ]; then
  state_mtime="$(stat -c %Y "$MANDATE_STATE_FILE" 2>/dev/null || stat -f %m "$MANDATE_STATE_FILE" 2>/dev/null || echo 0)"
fi
now="$(date +%s)"
cadence_seconds="$((cadence_hours * 3600))"

stale=0
[ "$((now - state_mtime))" -ge "$cadence_seconds" ] && stale=1

if ! queue="$(mandate_read_queue 2>/dev/null)"; then
  echo "mandate digest: queue is malformed; run /desk after repairing $MANDATE_QUEUE_FILE" >&2
  exit 0
fi

active="$(jq '[.[] | select(.state == "pending" or .state == "deferred")]' <<<"$queue")"
if [ -s "$MANDATE_STATE_FILE" ]; then
  cursor="$(
    jq -r '[.repos[]? | .previous_cursor // .cursor] | if length == 0 then "initial" else min end' \
      "$MANDATE_STATE_FILE" 2>/dev/null || echo initial
  )"
else
  cursor="initial"
fi

render_section() {
  heading="$1"
  kinds="$2"
  rows="$(
    jq -r --argjson kinds "$kinds" '
    .[]
    | select(.kind as $kind | $kinds | index($kind))
    | .repo + .ref + " " + .kind + " — "
      + (.mandate.reason // .mandate)
      + (if .state == "deferred" then " [deferred]" else "" end)
    ' <<<"$active"
  )"
  [ -n "$rows" ] || return 0
  echo "$heading"
  printf '%s\n' "$rows"
}

render_section "DECISIONS WAITING" '["tripwire","decision"]'
render_section "MOVED SINCE $cursor" '["moved"]'
render_section "STUCK" '["stuck"]'
render_section "DRIFT" '["drift"]'

state_rollups='[]'
dead_selectors='[]'
if [ -s "$MANDATE_STATE_FILE" ]; then
  state_rollups="$(
    jq -c '
      [
        .repos
        | to_entries[]
        | {
            repo: .key,
            notice: (.value.notice.text // null),
            unclassified: (.value.unclassified // 0)
          }
      ]
    ' "$MANDATE_STATE_FILE" 2>/dev/null || echo '[]'
  )"
  dead_selectors="$(
    jq -c '.dead_selectors // []' "$MANDATE_STATE_FILE" 2>/dev/null || echo '[]'
  )"
fi

jq -r '.[] | .notice // empty' <<<"$state_rollups"
jq -r '
  .[]
  | select(.unclassified > 0)
  | .repo + ": " + (.unclassified | tostring) + " unclassified — /desk triage"
' <<<"$state_rollups"
jq -r '
  .[]
  | if .repo == null
    then "dead selector — " + .source + " " + .selector
    else .repo + ": dead selector — " + .source + " " + .selector
    end
' <<<"$dead_selectors"

total_projects="$(jq '.projects | length' <<<"$config")"
troubled_projects="$(
  jq -n \
    --argjson active "$active" \
    --argjson rollups "$state_rollups" \
    --argjson dead "$dead_selectors" '
    [
      $active[].repo,
      ($rollups[] | select(.notice != null or .unclassified > 0) | .repo),
      ($dead[] | .repo // empty)
    ]
    | unique
    | length
  '
)"
nominal="$((total_projects - troubled_projects))"
[ "$nominal" -lt 0 ] && nominal=0
[ "$stale" -eq 1 ] && echo "STALE — mandate sweep overdue"
echo "$nominal projects nominal"
exit 0
