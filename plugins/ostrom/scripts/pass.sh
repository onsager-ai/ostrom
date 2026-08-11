#!/usr/bin/env bash
# Run one unattended delivery pass for a named role.
#
# systemd owns recurrence; this process owns one bounded Claude session. The
# outer lease is deliberately distinct from the protocol lease acquired by the
# skill itself, so timer overlap is rejected without making the skill block on
# its own wrapper.

set -uo pipefail
umask 077

usage() {
  echo "usage: pass.sh builder | gatekeeper" >&2
  exit 2
}

[ "$#" -eq 1 ] || usage
role="$1"
case "$role" in
  builder)
    prompt='/ostrom:work'
    permission_mode=auto
    # skills/work/SKILL.md step 2 acquires its protocol lease with
    # MANDATE_LEASE_NAME=builder.lease.
    inner_lease_name=builder.lease
    ;;
  gatekeeper)
    prompt='/ostrom:gatekeep'
    # The gatekeeper's authority is narrower than the builder's and its mode
    # is pending a separate operator decision. `manual` preserves the
    # pre-existing behaviour (every ask denied in this headless session, so
    # the pass fails closed at its first tool call) deliberately, so that
    # loosening it is an explicit act and not a side effect of this fix.
    permission_mode=manual
    # skills/gatekeep/SKILL.md step 2 acquires its protocol lease with no
    # MANDATE_LEASE_NAME override, which lands on lease.sh's default name,
    # sprint.lease -- a legacy name predating the newer <role>.lease
    # convention the builder uses, never renamed. Match what the skill
    # actually does, not the newer pattern.
    inner_lease_name=sprint.lease
    ;;
  *)
    usage
    ;;
esac

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=mandate-lib.sh
source "$SCRIPT_DIR/mandate-lib.sh"

CLAUDE_BIN="${CLAUDE_BIN:-$HOME/.local/bin/claude}"
ARM_FILE="$MANDATE_DATA_DIR/loop-armed"
SETTINGS="$MANDATE_DATA_DIR/roles/$role.settings.json"
RUN_DIR="$MANDATE_DATA_DIR/pass-runs/$role"
LEASE_NAME="$role-pass.lease"
INNER_LEASE_NAME="$inner_lease_name"
ID_FILE="$MANDATE_DATA_DIR/$role-pass-id"
WAKE_FILE="$MANDATE_DATA_DIR/$role-wake-counter"
# trace.sh keeps this path to itself; it offers no "where" or "watermark"
# subcommand, so it is duplicated here rather than adding one for a single
# caller. context.ts's readTrace() duplicates the same path on the doctor
# side for the same reason.
TRACE_FILE="$MANDATE_DATA_DIR/sprint.jsonl"

# The wall-clock timeout (systemd's TimeoutStartSec) is the pass's intended
# bound, because time maps directly to cost. A turn ceiling truncates on an
# axis nobody budgets against, so it remains only as a runaway-loop backstop
# -- set well above any turn count a real, working pass has been observed to
# need.
MAX_TURNS=200

# Total measured spend, across every role, that this pass's own UTC day may
# reach before further passes stand down for the rest of it. This is a
# budget for the system, not per role -- one role staying under it while
# another burns through it is still an unbounded day. MANDATE_DAILY_CAP_USD
# overrides the default for tuning without editing this script; anything
# that is not a plain number (unset, empty, typo'd) falls back to the
# default rather than leaving the loops uncapped on a bad override.
DAILY_CAP_USD=50
if [ -n "${MANDATE_DAILY_CAP_USD:-}" ]; then
  if parsed_cap="$(jq -n --arg v "$MANDATE_DAILY_CAP_USD" '$v | tonumber' 2>/dev/null)"; then
    DAILY_CAP_USD="$parsed_cap"
  fi
fi

log_note() {
  echo "ostrom $role pass: $*" >&2
}

# Absence is the durable kill switch. An invalid expiry also fails closed: a
# typo in a safety boundary must not silently become an indefinite arm.
if [ ! -f "$ARM_FILE" ]; then
  log_note "loop is disarmed; $ARM_FILE is absent"
  exit 0
fi
if [ -s "$ARM_FILE" ]; then
  arm_expiry="$(command cat "$ARM_FILE" 2>/dev/null)" || {
    log_note "loop is disarmed; arm state is unreadable"
    exit 0
  }
  case "$arm_expiry" in
    *$'\n'*)
      log_note "loop is disarmed; arm expiry must be one ISO-8601 timestamp"
      exit 0
      ;;
  esac
  if [[ ! "$arm_expiry" =~ ^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}(Z|[+-][0-9]{2}:[0-9]{2})$ ]]; then
    log_note "loop is disarmed; arm expiry is not a valid ISO-8601 timestamp"
    exit 0
  fi
  arm_expiry_epoch="$(date -d "$arm_expiry" +%s 2>/dev/null)" || {
    log_note "loop is disarmed; arm expiry is not a valid ISO-8601 timestamp"
    exit 0
  }
  if [ "$(date +%s)" -ge "$arm_expiry_epoch" ]; then
    log_note "loop is disarmed; arm expiry has passed"
    exit 0
  fi
fi

mkdir -p "$MANDATE_DATA_DIR"

# The short id is stable until its role state is removed. noclobber makes the
# first installation safe when two timer starts race on a new machine.
if [ ! -e "$ID_FILE" ]; then
  printf -v id_candidate '%04x%04x' "$RANDOM" "$RANDOM"
  (set -o noclobber; printf '%s\n' "$id_candidate" >"$ID_FILE") 2>/dev/null || true
fi
role_id="$(command cat "$ID_FILE" 2>/dev/null)" || {
  log_note "role id state is unreadable"
  exit 1
}
case "$role_id" in
  [0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f]) ;;
  *)
    log_note "role id state is invalid"
    exit 1
    ;;
esac

wake=0
if [ -e "$WAKE_FILE" ]; then
  wake="$(command cat "$WAKE_FILE" 2>/dev/null)" || {
    log_note "wake counter is unreadable"
    exit 1
  }
  case "$wake" in
    ''|*[!0-9]*)
      log_note "wake counter is invalid"
      exit 1
      ;;
  esac
fi
wake=$((wake + 1))
owner="$role-$role_id-wake$wake"

# Bash defers a trapped signal until a foreground child returns. Remember it
# across lease acquisition, establish whether this process won, and only then
# run the full cleanup path; this closes the acquisition-to-trap leak window.
pending_signal=""
pending_signal_status=0
defer_signal() {
  pending_signal="$1"
  pending_signal_status="$2"
}
trap 'defer_signal HUP 129' HUP
trap 'defer_signal INT 130' INT
trap 'defer_signal TERM 143' TERM

MANDATE_LEASE_NAME="$LEASE_NAME" \
  bash "$SCRIPT_DIR/lease.sh" acquire "$owner" >/dev/null
lease_status=$?
if [ "$lease_status" -eq 3 ]; then
  log_note "another pass already holds $LEASE_NAME; skipping"
  exit 0
fi
if [ "$lease_status" -ne 0 ]; then
  log_note "could not acquire $LEASE_NAME (rc=$lease_status)"
  exit "$lease_status"
fi

lease_acquired=1
pass_started=0
child_pid=""
child_spawned=0
outcome=""
outcome_reason=""
start_epoch="$(date +%s)"
log=""

read_cost() {
  measured_cost=null
  if [ -n "$log" ] && [ -f "$log" ]; then
    parsed_cost="$({
      jq -Rn '
        reduce inputs as $line
          (null;
            ($line | try fromjson catch null) as $event
            | if (($event | type) == "object" and $event.type == "result")
              then $event
              else .
              end)
        | if ((type == "object") and (.total_cost_usd | type) == "number")
          then .total_cost_usd
          else null
          end
      ' "$log" 2>/dev/null
    } || printf 'null\n')"
    if jq -e 'type == "number"' >/dev/null 2>&1 <<<"$parsed_cost"; then
      measured_cost="$parsed_cost"
    fi
  fi
  printf '%s\n' "$measured_cost"
}

# Sum cost_usd out of every pass-ended row (any role) whose ts falls on the
# given UTC day (a plain YYYY-MM-DD prefix match against the ISO-8601 ts
# trace.sh always writes). A line that fails to parse as JSON, or whose
# fact.cost_usd is missing or not a number, contributes 0 rather than
# aborting the reduce: a pass whose cost was never measured is a gap in the
# record, and a cap that refuses to run because one malformed row exists
# would be a worse failure than a slightly under-counted day.
daily_spend_usd() {
  spend_day="$1"
  [ -f "$TRACE_FILE" ] || { printf '0\n'; return; }
  jq -Rn --arg day "$spend_day" '
    reduce inputs as $line
      (0;
        ($line | try fromjson catch null) as $event
        | if (($event | type) == "object")
            and ($event.kind == "pass-ended")
            and (($event.ts | type) == "string")
            and ($event.ts | startswith($day))
          then . + (
            ($event.fact.cost_usd?) as $cost
            | if ($cost | type) == "number" then $cost else 0 end
          )
          else .
          end
      )
  ' "$TRACE_FILE"
}

# The wrapper's own pass-started row (appended below, before the child is
# spawned) proves only that this process ran; it says nothing about whether
# the protocol it invoked ever began. work/SKILL.md and gatekeep/SKILL.md
# step 2 append their own pass-started row, under an independently minted
# owner, only after the inner session acquires its protocol lease -- so "did
# the protocol begin" reduces to "did a second pass-started row, not ours,
# land in the trace after this one". That owner cannot be predicted or
# matched by name (see release_inner_lease's comment for why), so presence is
# judged by kind and role prefix alone, scanned only over the lines appended
# after $trace_watermark -- never the wrapper's own row, which precedes it.
inner_pass_started() {
  [ -f "$TRACE_FILE" ] || { printf 'false\n'; return; }
  tail -n +"$((trace_watermark + 1))" "$TRACE_FILE" 2>/dev/null | jq -Rn \
    --arg prefix "$role-" \
    --arg owner "$owner" '
      reduce inputs as $line
        (false;
          . or (
            ($line | try fromjson catch null) as $event
            | (($event | type) == "object")
              and ($event.kind == "pass-started")
              and ((try ($event.fact.owner) catch null) as $inner_owner
                | ($inner_owner | type) == "string"
                  and ($inner_owner | startswith($prefix))
                  and ($inner_owner != $owner))
          )
        )
    '
}

# The inner lease (INNER_LEASE_NAME) is acquired by the Claude session this
# script spawns, as step 2 of its own protocol -- one layer inside the outer
# $LEASE_NAME this script acquires for itself. When the child is killed
# before it reaches its own release step -- timeout, signal, max-turns,
# crash -- that lease outlives it for its full TTL and silently stalls the
# next pass, which is indistinguishable from an idle loop.
#
# Reclaim it here, but only when its started_at is at or after this pass's
# own start_epoch: that is the one sound proof the held lease belongs to
# *our* child and not to a concurrent interactive session of the same role,
# whose lease must never be stolen. The inner session mints its own owner
# string (a different id and wake counter from this wrapper's $owner), so
# ownership cannot be checked by name -- only by timestamp. Release always
# goes through lease.sh, using the owner string read off the held lease, so
# lease.sh's own owner check still applies; this never deletes the lease
# file directly and never forces past that check.
#
# The timestamp test alone is not sufficient, because finish() also runs on
# early-exit paths -- the pass-started trace append failing, $SETTINGS
# missing, $CLAUDE_BIN not executable -- where this pass never reached the
# point of spawning a child at all. On those paths there is no child of ours
# that could hold the inner lease, so any lease found there, however fresh
# its started_at, belongs to a concurrent interactive session and must be
# left alone. Reclamation therefore requires both proofs together: a child
# spawned by this pass, and a held lease that started at or after this pass
# began.
release_inner_lease() {
  if [ "$child_spawned" -eq 0 ]; then
    log_note "this pass never spawned a child; any held inner lease $INNER_LEASE_NAME cannot be ours, leaving it alone"
    return 0
  fi

  inner_lease_json="$(
    MANDATE_LEASE_NAME="$INNER_LEASE_NAME" \
      bash "$SCRIPT_DIR/lease.sh" status 2>/dev/null
  )" || {
    return 0
  }

  inner_owner="$(jq -r '.owner' <<<"$inner_lease_json" 2>/dev/null)" || return 0
  inner_started_at="$(jq -r '.started_at' <<<"$inner_lease_json" 2>/dev/null)" || return 0
  case "$inner_started_at" in
    ''|*[!0-9]*)
      log_note "inner lease $INNER_LEASE_NAME has an unreadable started_at; leaving it alone"
      return 0
      ;;
  esac

  if [ "$inner_started_at" -lt "$start_epoch" ]; then
    log_note "inner lease $INNER_LEASE_NAME started at $inner_started_at, before this pass's own start at $start_epoch; leaving it to its own owner"
    return 0
  fi

  log_note "releasing inner lease $INNER_LEASE_NAME held by $inner_owner (started_at=$inner_started_at, pass start=$start_epoch)"
  if ! MANDATE_LEASE_NAME="$INNER_LEASE_NAME" \
    bash "$SCRIPT_DIR/lease.sh" release "$inner_owner" >/dev/null 2>&1; then
    log_note "could not release inner lease $INNER_LEASE_NAME held by $inner_owner"
    return 1
  fi
  return 0
}

finish() {
  saved_status=$?
  trap - EXIT HUP INT TERM

  if [ "$lease_acquired" -eq 1 ]; then
    if [ "$pass_started" -eq 1 ]; then
      [ -n "$outcome" ] || {
        if [ "$saved_status" -eq 0 ]; then
          outcome=completed
        else
          outcome=failed
        fi
      }
      end_epoch="$(date +%s)"
      duration_seconds=$((end_epoch - start_epoch))
      [ "$duration_seconds" -ge 0 ] || duration_seconds=0
      cost_usd="$(read_cost)"
      end_fact="$(
        jq -cn \
          --arg owner "$owner" \
          --arg outcome "$outcome" \
          --argjson cost_usd "$cost_usd" \
          --argjson duration_seconds "$duration_seconds" \
          --arg reason "$outcome_reason" \
          '{owner: $owner, outcome: $outcome, cost_usd: $cost_usd,
            duration_seconds: $duration_seconds}
            + (if $reason == "" then {} else {reason: $reason} end)'
      )"
      if ! bash "$SCRIPT_DIR/trace.sh" append pass-ended "$end_fact" '{}' >/dev/null; then
        log_note "could not append pass-ended"
        [ "$saved_status" -ne 0 ] || saved_status=1
      fi
    fi

    # Reclaim the inner lease before releasing the outer one, so the two are
    # never both briefly free in a way that lets a new pass start against
    # half-cleared state.
    if ! release_inner_lease; then
      [ "$saved_status" -ne 0 ] || saved_status=1
    fi

    # Lease release is attempted after every acquired path, even when trace
    # finalization failed. A leaked lease turns a failed pass into silent idle.
    if ! MANDATE_LEASE_NAME="$LEASE_NAME" \
      bash "$SCRIPT_DIR/lease.sh" release "$owner" >/dev/null; then
      log_note "could not release $LEASE_NAME"
      [ "$saved_status" -ne 0 ] || saved_status=1
    fi
    lease_acquired=0
  fi

  if [ -d "$RUN_DIR" ]; then
    # Transcripts exist only to diagnose a bad unattended run. Keep a bounded
    # local tail because their contents can be sensitive and are never reports.
    ls -1t "$RUN_DIR"/*.jsonl 2>/dev/null | tail -n +31 | xargs -r rm -f
  fi
  exit "$saved_status"
}

on_signal() {
  signal_name="$1"
  signal_status="$2"
  trap '' HUP INT TERM
  if [ "$signal_name" = TERM ]; then
    # systemd uses SIGTERM when TimeoutStartSec expires.
    outcome=timed-out
  else
    outcome=failed
  fi
  if [ -n "$child_pid" ] && kill -0 "$child_pid" 2>/dev/null; then
    kill -TERM "$child_pid" 2>/dev/null || true
    wait "$child_pid" 2>/dev/null || true
    child_pid=""
  fi
  exit "$signal_status"
}

trap finish EXIT
trap 'on_signal HUP 129' HUP
trap 'on_signal INT 130' INT
trap 'on_signal TERM 143' TERM
if [ -n "$pending_signal" ]; then
  on_signal "$pending_signal" "$pending_signal_status"
fi

# Persist only the winning pass's wake. A contender that backed off above does
# not consume a wake number and therefore leaves no false gap in role history.
wake_tmp="$WAKE_FILE.tmp.$$"
printf '%s\n' "$wake" >"$wake_tmp"
mv "$wake_tmp" "$WAKE_FILE"

start_fact="$(jq -cn --arg owner "$owner" '{owner: $owner}')"
if ! bash "$SCRIPT_DIR/trace.sh" append pass-started "$start_fact" '{}' >/dev/null; then
  log_note "could not append pass-started"
  outcome=failed
  exit 1
fi
pass_started=1

# Watermark the trace at the line this pass's own row lands on, immediately
# after writing it and before anything else can append -- everything from
# here on is either this pass's child or a concurrent pass of some other
# role, never a race against our own write above.
trace_watermark="$(wc -l <"$TRACE_FILE" 2>/dev/null | tr -d '[:space:]')"
[ -n "$trace_watermark" ] || trace_watermark=0

[ -f "$SETTINGS" ] || {
  log_note "$SETTINGS missing"
  outcome=failed
  exit 1
}
[ -x "$CLAUDE_BIN" ] || {
  log_note "$CLAUDE_BIN not executable"
  outcome=failed
  exit 1
}

# The daily spend cap sits here, after both leases are already accounted for
# and before the one action that actually costs money: spawning the child.
# A capped pass exits through the same finish() path as the SETTINGS and
# CLAUDE_BIN checks just above -- $lease_acquired is 1, $pass_started is 1 --
# so it releases the outer lease and never spawns anything to hold the inner
# one either way; no separate cleanup is needed to keep this pass lease-free.
#
# This deliberately never touches $ARM_FILE. That file is the principal's
# kill switch -- the one control surface they use to start and stop the
# loops -- and a cap that disarmed the loops to enforce a budget would take
# that control surface away in the name of protecting it. Standing down for
# the rest of the UTC day is self-clearing at midnight and leaves $ARM_FILE
# meaning exactly what it meant before this cap existed. Do not "simplify"
# this into a disarm.
now_epoch="${MANDATE_NOW_EPOCH:-$(date +%s)}"
case "$now_epoch" in
  ''|*[!0-9]*) now_epoch="$(date +%s)" ;;
esac
today="$(date -u -d "@$now_epoch" +%Y-%m-%d)"
daily_total_usd="$(daily_spend_usd "$today")"
if [ "$(jq -n --argjson total "$daily_total_usd" --argjson cap "$DAILY_CAP_USD" '$total >= $cap')" = true ]; then
  log_note "daily spend cap reached: \$$daily_total_usd of \$$DAILY_CAP_USD spent today ($today UTC, all roles); skipping this pass"
  outcome=no-op
  outcome_reason=daily-cap
  exit 0
fi

mkdir -p "$RUN_DIR"
stamp="$(date -u +%Y%m%dT%H%M%SZ)"
log="$RUN_DIR/$stamp-$owner.jsonl"

"$CLAUDE_BIN" \
  --print \
  --settings "$SETTINGS" \
  --permission-mode "$permission_mode" \
  --output-format stream-json \
  --verbose \
  --max-turns "$MAX_TURNS" \
  "$prompt" >"$log" 2>&1 &
child_pid=$!
child_spawned=1
wait "$child_pid"
run_status=$?
child_pid=""

if [ "$(inner_pass_started)" = "true" ]; then
  # The protocol took ownership; today's exit-code-derived outcome applies
  # exactly as it did before this pass began telling no-op apart from it.
  if [ "$run_status" -eq 0 ]; then
    outcome=completed
  else
    outcome=failed
    log_note "Claude run failed (rc=$run_status); transcript at $log"
  fi
elif [ "$run_status" -eq 0 ]; then
  # claude exited cleanly, but no session-owned pass-started row ever
  # followed ours: the protocol never began. This is the exact failure #73
  # exists to surface. "blocked" is the default reason, since this wrapper
  # cannot read the child's transcript to say more -- but if the inner lease
  # is currently held by a session that started before this pass did, that
  # is a knowable, more specific cause: the child found its own protocol
  # lease already contended and stopped, exactly as its instructions require
  # (see release_inner_lease's comment for the same started_at test, used
  # there to decide reclaim rather than to name a reason here).
  outcome=no-op
  outcome_reason=blocked
  inner_status_json="$(
    MANDATE_LEASE_NAME="$INNER_LEASE_NAME" \
      bash "$SCRIPT_DIR/lease.sh" status 2>/dev/null
  )" || inner_status_json=""
  if [ -n "$inner_status_json" ]; then
    inner_status_started_at="$(jq -r '.started_at' <<<"$inner_status_json" 2>/dev/null)"
    case "$inner_status_started_at" in
      ''|*[!0-9]*) ;;
      *)
        [ "$inner_status_started_at" -lt "$start_epoch" ] && outcome_reason=lease-held
        ;;
    esac
  fi
  log_note "claude exited 0 but the protocol never started (reason=$outcome_reason); recording no-op"
else
  outcome=failed
  log_note "Claude run failed (rc=$run_status) and the protocol never started; transcript at $log"
fi
exit "$run_status"
