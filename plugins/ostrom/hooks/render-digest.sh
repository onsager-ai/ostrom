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

# Decisions taken lead the digest: a returning principal should read what
# happened, not what is stuck. "Since last read" needs its own watermark —
# the trace is an append-only log, so marking individual records read the way
# a state.json notice is marked `reported` would mean rewriting sprint.jsonl,
# which nothing else in this subsystem does. A small sentinel file instead,
# parallel to the .tap-$today gate below, records only the moment of the last
# render.
trace_file="$MANDATE_DATA_DIR/sprint.jsonl"
decisions_watermark_file="$MANDATE_DATA_DIR/.digest-decisions-read"
digest_now="${MANDATE_DIGEST_TIME:-$(date -u +%Y-%m-%dT%H:%M:%SZ)}"

decisions_since="1970-01-01T00:00:00Z"
if [ -s "$decisions_watermark_file" ]; then
  candidate_since="$(head -n 1 "$decisions_watermark_file" 2>/dev/null || true)"
  case "$candidate_since" in
    [0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9]Z)
      decisions_since="$candidate_since"
      ;;
  esac
fi

# A trace record's shape is never trusted blindly here. This reads the file
# directly rather than through `trace.sh read`, deliberately: that command
# aborts entirely on the first malformed record anywhere in the trace, which
# would take down this section over a corrupt line from an unrelated kind. A
# non-JSON line is skipped instead of aborting the read, and every fact field
# the renderer touches falls back to a placeholder rather than erroring, so a
# `decision-taken` row missing a field degrades this section instead of the
# hook.
decisions_json='[]'
if [ -s "$trace_file" ]; then
  decisions_json="$(
    jq -R -c '
      try fromjson catch empty
      | select(type == "object" and .kind == "decision-taken")
      | {
          ts: (if (.ts | type) == "string" then .ts else "" end),
          repo: (if ((.fact // {}).repo | type) == "string"
                 then .fact.repo else "(repo unknown)" end),
          ref: (if ((.fact // {}).ref | type) == "string"
                then .fact.ref else "" end),
          decision: (if ((.fact // {}).decision | type) == "string"
                     and (((.fact // {}).decision | length) > 0)
                     then .fact.decision else "(decision unavailable)" end),
          reversal: (if ((.fact // {}).reversal | type) == "string"
                     and (((.fact // {}).reversal | length) > 0)
                     then .fact.reversal else "reversal not recorded" end),
          reason: (if (((.narration // {}).reason) | type) == "string"
                   then .narration.reason else "" end)
        }
    ' "$trace_file" 2>/dev/null | jq -s '.' 2>/dev/null
  )"
  [ -n "$decisions_json" ] || decisions_json='[]'
fi

decisions_rows="$(
  jq -r --arg since "$decisions_since" '
    [ .[] | select(.ts > $since) ] | sort_by(.ts) | reverse
    | .[]
    | (.repo + .ref) as $ref
    | $ref + "  " + .decision
      + (if (.reason | length) > 0 then " — " + .reason else "" end)
      + "  [reversal: " + .reversal + "]"
  ' <<<"$decisions_json" 2>/dev/null || true
)"

if [ -n "$decisions_rows" ]; then
  echo "DECISIONS TAKEN"
  printf '%s\n' "$decisions_rows"
else
  echo "DECISIONS TAKEN: nothing since your last read"
fi

render_section "DECISIONS WAITING" '["tripwire","decision"]'
render_section "MOVED SINCE $cursor" '["moved"]'
render_section "STUCK" '["stuck"]'
render_section "DRIFT" '["drift"]'
render_section "MERGE GATE FAULTS" '["merge-gate-fault"]'
parked_count="$(jq '[.[] | select(.kind == "parked")] | length' <<<"$active")"
[ "$parked_count" -eq 0 ] || echo "$parked_count parked"

unresolvable_repositories='[]'
if [ -s "$MANDATE_STATE_FILE" ]; then
  unresolvable_repositories="$(
    jq -c '[
      (.unresolvable_repositories // [])[]
      | select(type == "string" and length > 0)
    ] | unique' "$MANDATE_STATE_FILE" 2>/dev/null || echo '[]'
  )"
fi
if [ "$(jq 'length' <<<"$unresolvable_repositories")" -gt 0 ]; then
  echo "UNDISPATCHABLE REPOSITORIES"
  jq -r '.[] + " — source repository not found under search_roots"' \
    <<<"$unresolvable_repositories"
fi

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
            merge_gate_faults: (.value.merge_gate_fault_count // 0),
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
jq -r '
  .[]
  | select(.merge_gate_faults > 0)
  | .repo + ": " + (.merge_gate_faults | tostring)
    + (if .merge_gate_faults == 1 then " merge gate fault" else " merge gate faults" end)
    + " — /ostrom:desk triage"
' <<<"$state_rollups"

total_projects="$(jq '.projects | length' <<<"$config")"
troubled_projects="$(
  jq --argjson unresolvable "$unresolvable_repositories" '
    ([
       .[]
       | select(.kind | IN("tripwire", "decision", "drift", "stuck", "merge-gate-fault"))
       | .repo
     ] + $unresolvable)
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

# Advance the decisions watermark only here, once the digest has actually
# rendered a full pass over the trace. A failed write is a quiet loss of the
# cursor, never a failed SessionStart; the next render simply re-shows
# whatever this one would have hidden.
printf '%s\n' "$digest_now" >"$decisions_watermark_file" 2>/dev/null || true

exit 0
