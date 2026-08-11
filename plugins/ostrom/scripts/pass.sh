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
    ;;
  gatekeeper)
    prompt='/ostrom:gatekeep'
    # The gatekeeper's authority is narrower than the builder's and its mode
    # is pending a separate operator decision. `manual` preserves the
    # pre-existing behaviour (every ask denied in this headless session, so
    # the pass fails closed at its first tool call) deliberately, so that
    # loosening it is an explicit act and not a side effect of this fix.
    permission_mode=manual
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
ID_FILE="$MANDATE_DATA_DIR/$role-pass-id"
WAKE_FILE="$MANDATE_DATA_DIR/$role-wake-counter"

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
outcome=""
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
          '{owner: $owner, outcome: $outcome, cost_usd: $cost_usd,
            duration_seconds: $duration_seconds}'
      )"
      if ! bash "$SCRIPT_DIR/trace.sh" append pass-ended "$end_fact" '{}' >/dev/null; then
        log_note "could not append pass-ended"
        [ "$saved_status" -ne 0 ] || saved_status=1
      fi
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

mkdir -p "$RUN_DIR"
stamp="$(date -u +%Y%m%dT%H%M%SZ)"
log="$RUN_DIR/$stamp-$owner.jsonl"

"$CLAUDE_BIN" \
  --print \
  --settings "$SETTINGS" \
  --permission-mode "$permission_mode" \
  --output-format stream-json \
  --verbose \
  --max-turns 40 \
  "$prompt" >"$log" 2>&1 &
child_pid=$!
wait "$child_pid"
run_status=$?
child_pid=""

if [ "$run_status" -eq 0 ]; then
  outcome=completed
else
  outcome=failed
  log_note "Claude run failed (rc=$run_status); transcript at $log"
fi
exit "$run_status"
