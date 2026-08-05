#!/usr/bin/env bash
# SessionStart hook: render the last durable sweep as an exception digest.
#
# A machine with no private mandates.yaml emits nothing and exits 0. Once
# configured, this hook reads local files only, acknowledges rendered notices,
# and never makes a network call.

set -u
umask 077

# The digest is emitted twice: as `systemMessage` so Claude Code DISPLAYS it to
# the operator, and as `additionalContext` so the assistant holds the same queue
# without being told. Plain stdout would only do the second — the assistant would
# know the portfolio and the human would not, which inverts the point of a digest
# whose whole purpose is that the operator stops being an outsider to their own
# projects.
#
# Everything below still `echo`s normally; stdout is buffered and wrapped once on
# exit, so every early-return path (unconfigured machine, missing jq, malformed
# queue) stays correct and silent.
_digest_buf="$(mktemp)"
exec 3>&1 1>"$_digest_buf"

_emit_digest() {
  exec 1>&3 3>&-
  local body
  body="$(cat "$_digest_buf" 2>/dev/null)"
  rm -f "$_digest_buf"
  [ -n "$body" ] || return 0
  if command -v jq >/dev/null 2>&1; then
    jq -n --arg m "$body" '{
      systemMessage: $m,
      hookSpecificOutput: {
        hookEventName: "SessionStart",
        additionalContext: $m
      }
    }'
  else
    # No jq: fall back to plain stdout rather than emitting nothing. Context-only
    # delivery beats a silent hook.
    printf '%s\n' "$body"
  fi
}
trap _emit_digest EXIT

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
now="${MANDATE_NOW_EPOCH:-$(date +%s)}"
cadence_seconds="$((cadence_hours * 3600))"

stale=0
[ "$((now - state_mtime))" -ge "$cadence_seconds" ] && stale=1

if ! queue="$(mandate_read_queue 2>/dev/null)"; then
  echo "mandate digest: queue is malformed; run /ostrom:desk after repairing $MANDATE_QUEUE_FILE" >&2
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
    def title:
      if (((.title // "") | type) == "string")
        and (((.title // "") | length) > 0)
      then .title
      else "(title unavailable)"
      end;
    def truncate($text; $width):
      if ($text | length) <= $width then $text
      elif $width <= 1 then "…"
      else $text[0:$width - 1] + "…"
      end;
    def essential_reason:
      sub("; open PR passed CI$"; "")
      | sub("; no movement for [0-9]+ days$"; "");
    .[]
    | select(.kind as $kind | $kinds | index($kind))
    | . as $row
    | (.mandate.reason // .mandate) as $stored_reason
    | (
        if .kind == "moved"
        then $stored_reason | sub("; updated since the read cursor$"; "")
        else $stored_reason
        end
      ) as $reason
    | (if .state == "deferred" then " [deferred]" else "" end) as $suffix
    | (.repo + .ref) as $ref
    | ($row | title) as $title
    | (100 - (($ref | length) + 2 + 3 + ($suffix | length))) as $content_width
    | ([
        ($title | length),
        ([45, ($content_width - ($reason | length))] | max)
      ] | min) as $title_width
    | ([
        $content_width - $title_width,
        ($reason | essential_reason | length),
        1
      ] | max) as $reason_width
    | $ref + "  "
      + truncate($title; $title_width)
      + " — " + truncate($reason; $reason_width) + $suffix
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
if [ -s "$MANDATE_STATE_FILE" ]; then
  state_rollups="$(
    jq -c '
      [
        .repos
        | to_entries[]
        | {
            repo: .key,
            notice: (
              if .value.notice != null
                and ((.value.notice.reported // false) | not)
              then .value.notice.text
              else null
              end
            ),
            unclassified: (.value.unclassified // 0),
            item_cap: (.value.item_cap // null)
          }
      ]
    ' "$MANDATE_STATE_FILE" 2>/dev/null || echo '[]'
  )"
fi

jq -r '.[] | .notice // empty' <<<"$state_rollups"
jq -r '
  .[]
  | select(.item_cap != null)
  | .repo + ": item cap reached (" + (.item_cap | tostring)
    + ") — sweep may be incomplete"
' <<<"$state_rollups"
jq -r '
  .[]
  | select(.unclassified > 0)
  | .repo + ": " + (.unclassified | tostring) + " unclassified — /ostrom:desk triage"
' <<<"$state_rollups"

total_projects="$(jq '.projects | length' <<<"$config")"
troubled_projects="$(
  jq '
    [
      .[]
      | select(.kind | IN("tripwire", "decision", "drift", "stuck"))
      | .repo
    ]
    | unique
    | length
  ' <<<"$active"
)"
nominal="$((total_projects - troubled_projects))"
[ "$nominal" -lt 0 ] && nominal=0
local_drift="$(bash "$PLUGIN_ROOT/scripts/local-drift.sh" --local-only 2>/dev/null || true)"
[ -n "$local_drift" ] && echo "LOCAL DRIFT — run mandate local-drift.sh for details"
[ "$stale" -eq 1 ] && echo "STALE — mandate sweep overdue"
echo "$nominal projects nominal"

# The digest asks for synthesis once per local day. A failed sentinel write is
# a quiet loss of the prompt, never a failed SessionStart.
today="${MANDATE_TODAY:-$(date +%Y-%m-%d)}"
case "$today" in
  [0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9])
    tap="$MANDATE_DATA_DIR/.tap-$today"
    if (set -o noclobber; : >"$tap") 2>/dev/null; then
      echo
      echo "BRIEF"
      echo "Produce today's /ostrom:brief now. Separate blocked on you from blocked on no one; propose only. /ostrom:desk remains the sole decision surface."
    fi
    ;;
esac

# Baselines and mandate changes are news, not permanent digest content.
# Preserve the sweep-owned mtime because it is the cadence stamp.
if [ -s "$MANDATE_STATE_FILE" ] && jq -e '
  any(.repos[]?;
    .notice != null and ((.notice.reported // false) | not)
  )
' "$MANDATE_STATE_FILE" >/dev/null 2>&1; then
  notice_state="$(mktemp "$MANDATE_DATA_DIR/.state-notices.XXXXXX")"
  if jq -S '
    (.repos[]?.notice
      | select(. != null and ((.reported // false) | not))
    ).reported = true
  ' "$MANDATE_STATE_FILE" >"$notice_state"; then
    touch -r "$MANDATE_STATE_FILE" "$notice_state"
    mv "$notice_state" "$MANDATE_STATE_FILE"
  else
    rm -f "$notice_state"
  fi
fi
exit 0
