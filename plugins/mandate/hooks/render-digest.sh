#!/usr/bin/env bash
# SessionStart hook: run the due daily sweep, then render a fixed digest.
#
# A machine with no private mandates.yaml emits nothing and exits 0. Once
# configured, operational failures go to stderr but never break SessionStart;
# the last durable queue still renders.

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

if [ "${MANDATE_SKIP_SWEEP:-0}" != "1" ] && [ "$((now - state_mtime))" -ge "$cadence_seconds" ]; then
  if ! CLAUDE_PLUGIN_ROOT="$PLUGIN_ROOT" bash "$PLUGIN_ROOT/scripts/sweep.sh" >/dev/null; then
    echo "mandate digest: daily sweep failed; rendering the last durable queue" >&2
  fi
fi

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

render_rows() {
  kinds="$1"
  jq -r --argjson kinds "$kinds" '
    .[]
    | select(.kind as $kind | $kinds | index($kind))
    | .repo + .ref + " " + .kind + " — "
      + (.mandate.reason // .mandate)
      + (if .state == "deferred" then " [deferred]" else "" end)
  ' <<<"$active"
}

echo "DECISIONS WAITING"
render_rows '["tripwire","decision"]'
echo "MOVED SINCE $cursor"
render_rows '["moved"]'
echo "STUCK"
render_rows '["stuck"]'
echo "DRIFT"
render_rows '["drift"]'

total_projects="$(jq '.projects | length' <<<"$config")"
troubled_projects="$(jq '[.[].repo] | unique | length' <<<"$active")"
nominal="$((total_projects - troubled_projects))"
[ "$nominal" -lt 0 ] && nominal=0
echo "$nominal projects nominal"
exit 0
