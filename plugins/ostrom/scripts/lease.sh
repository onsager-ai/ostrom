#!/usr/bin/env bash
# Coordinate builder wakes with a machine-local sprint lease.

set -euo pipefail
umask 077

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=mandate-lib.sh
source "$SCRIPT_DIR/mandate-lib.sh"

command -v jq >/dev/null 2>&1 || { echo "mandate lease: jq is required" >&2; exit 1; }

LEASE_FILE="$MANDATE_DATA_DIR/sprint.lease"
LEASE_GUARD="$MANDATE_DATA_DIR/.sprint.lease.guard"
lease_guard_held=0

usage() {
  echo "usage: lease.sh acquire <owner> [ttl-seconds] | release <owner> | status" >&2
  exit 2
}

lease_now() {
  now="${MANDATE_LEASE_NOW_EPOCH:-$(date +%s)}"
  case "$now" in
    ''|*[!0-9]*)
      echo "mandate lease: current time must be Unix seconds" >&2
      return 2
      ;;
  esac
  printf '%s\n' "$now"
}

lease_load() {
  [ -f "$LEASE_FILE" ] || return 1
  lease_json="$(command cat "$LEASE_FILE")" || return 1
  jq -e '
    . as $lease
    | type == "object"
    and (keys == ["expires_at", "owner", "started_at"])
    and (.owner | type == "string" and length > 0)
    and (.started_at | type == "number" and . >= 0 and . == floor)
    and (.expires_at | type == "number" and . >= 0 and . == floor)
    and ($lease.expires_at >= $lease.started_at)
  ' >/dev/null <<<"$lease_json"
}

lease_install() {
  install_owner="$1"
  install_started="$2"
  install_expires="$3"
  install_json="$(
    jq -cn \
      --arg owner "$install_owner" \
      --argjson started_at "$install_started" \
      --argjson expires_at "$install_expires" \
      '{owner: $owner, started_at: $started_at, expires_at: $expires_at}'
  )"

  # noclobber opens the absent path with O_EXCL semantics. Exactly one
  # concurrent builder can create it; a loser cannot alter the held lease.
  if (set -o noclobber; printf '%s\n' "$install_json" >"$LEASE_FILE") 2>/dev/null; then
    printf '%s\n' "$install_json"
    return 0
  fi
  return 1
}

# Only expiry reclamation and owner release delete a lease. Serialize those
# paths so a stale reader can never unlink a replacement lease. Initial
# acquisition remains the O_EXCL operation on LEASE_FILE itself.
lease_guard_acquire() {
  guard_token="$$.$RANDOM.$RANDOM"
  if (set -o noclobber; printf '%s\n' "$guard_token" >"$LEASE_GUARD") 2>/dev/null; then
    lease_guard_held=1
    trap 'lease_guard_release' EXIT HUP INT TERM
    return 0
  fi
  return 1
}

lease_guard_release() {
  if [ "$lease_guard_held" -eq 1 ]; then
    rm -f "$LEASE_GUARD"
    lease_guard_held=0
  fi
  trap - EXIT HUP INT TERM
}

action="${1:-}"

case "$action" in
  acquire)
    [ "$#" -eq 2 ] || [ "$#" -eq 3 ] || usage
    owner="$2"
    ttl="${3:-${MANDATE_LEASE_TTL_SECONDS:-3600}}"
    [ -n "$owner" ] || usage
    case "$ttl" in
      ''|*[!0-9]*|0)
        echo "mandate lease: ttl-seconds must be a positive integer" >&2
        exit 2
        ;;
    esac
    now="$(lease_now)" || exit
    expires=$((now + ttl))
    mkdir -p "$MANDATE_DATA_DIR"

    if lease_install "$owner" "$now" "$expires"; then
      exit 0
    fi
    if ! lease_load; then
      echo "mandate lease: lease is held or unreadable" >&2
      exit 3
    fi
    held_expires="$(jq -r '.expires_at' <<<"$lease_json")"
    if [ "$now" -lt "$held_expires" ]; then
      echo "mandate lease: lease is held" >&2
      exit 3
    fi

    if ! lease_guard_acquire; then
      echo "mandate lease: lease reclamation is already in progress" >&2
      exit 3
    fi
    # Re-read under the guard. Another builder may have reclaimed the lease
    # after the first expiry check and before this builder acquired the guard.
    if ! lease_load; then
      lease_guard_release
      echo "mandate lease: lease changed during reclamation" >&2
      exit 3
    fi
    held_expires="$(jq -r '.expires_at' <<<"$lease_json")"
    if [ "$now" -lt "$held_expires" ]; then
      lease_guard_release
      echo "mandate lease: lease is held" >&2
      exit 3
    fi

    rm -f "$LEASE_FILE"
    if lease_install "$owner" "$now" "$expires"; then
      lease_guard_release
      exit 0
    fi
    lease_guard_release
    echo "mandate lease: lease was acquired concurrently" >&2
    exit 3
    ;;
  release)
    [ "$#" -eq 2 ] || usage
    owner="$2"
    [ -n "$owner" ] || usage
    if ! lease_guard_acquire; then
      echo "mandate lease: lease mutation is already in progress" >&2
      exit 3
    fi
    if ! lease_load; then
      lease_guard_release
      echo "mandate lease: no readable lease" >&2
      exit 3
    fi
    held_owner="$(jq -r '.owner' <<<"$lease_json")"
    if [ "$owner" != "$held_owner" ]; then
      lease_guard_release
      echo "mandate lease: owner mismatch" >&2
      exit 3
    fi
    rm -f "$LEASE_FILE"
    lease_guard_release
    ;;
  status)
    [ "$#" -eq 1 ] || usage
    if ! lease_load; then
      echo "mandate lease: no readable lease" >&2
      exit 3
    fi
    printf '%s\n' "$lease_json"
    ;;
  *)
    usage
    ;;
esac
