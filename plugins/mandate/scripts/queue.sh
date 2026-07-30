#!/usr/bin/env bash
# Inspect the mandate state, or mutate the private queue after a /desk decision.

set -euo pipefail
umask 077

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=mandate-lib.sh
source "$SCRIPT_DIR/mandate-lib.sh"

command -v jq >/dev/null 2>&1 || { echo "mandate queue: jq is required" >&2; exit 1; }

action="${1:-list}"
id="${2:-}"

if [ "$action" = "lint" ]; then
  if [ ! -s "$MANDATE_STATE_FILE" ]; then
    echo "mandate lint: no sweep state at $MANDATE_STATE_FILE" >&2
    exit 2
  fi
  jq -r '
    (.dead_selectors // [])[]
    | if .repo == null
      then "unmatched in last sweep — " + .source + " " + .selector
      else .repo + ": unmatched in last sweep — " + .source + " " + .selector
      end
  ' "$MANDATE_STATE_FILE"
  exit 0
fi

queue="$(mandate_read_queue)" || {
  echo "mandate queue: cannot read $MANDATE_QUEUE_FILE" >&2
  exit 2
}

case "$action" in
  list)
    jq -c '.[] | select(.state == "pending" or .state == "deferred")' <<<"$queue"
    ;;
  approve|reject|defer)
    [ -n "$id" ] || { echo "usage: queue.sh $action <id>" >&2; exit 2; }
    row="$(
      jq -c --arg id "$id" '
        first(.[] | select(
          .id == $id and (.state == "pending" or .state == "deferred")
        )) // empty
      ' <<<"$queue"
    )"
    [ -n "$row" ] || {
      echo "mandate queue: no pending or deferred item with id $id" >&2
      exit 3
    }

    mkdir -p "$MANDATE_DATA_DIR"
    tmp="$(mktemp "$MANDATE_DATA_DIR/.queue.XXXXXX")"
    trap 'rm -f "$tmp"' EXIT
    case "$action" in
      approve)
        row_repo="$(jq -r '.repo' <<<"$row")"
        if [ -s "$MANDATE_STATE_FILE" ] &&
          jq -e --arg repo "$row_repo" '.repos[$repo].policy.paused == true' \
            "$MANDATE_STATE_FILE" >/dev/null 2>&1; then
          echo "mandate queue: $row_repo is paused; CI drift cannot mint a handoff token" >&2
          exit 4
        fi
        jq -c --arg id "$id" '
          map(if .id == $id then .state = "approved" else . end)[]' <<<"$queue" >"$tmp"
        mandate_write_if_changed "$tmp" "$MANDATE_QUEUE_FILE"
        rm -f "$tmp"
        trap - EXIT
        approved="$(jq -c '.state = "approved"' <<<"$row")"
        printf '%s\n' "$approved"
        jq -r '
          "HANDOFF " + .repo + " " + .ref
          + " — invoke /handoff with approval token mandate:" + .id
          + "; mandate: " + (.mandate.reason // .mandate)
        ' <<<"$approved"
        ;;
      reject)
        jq -c --arg id "$id" 'map(select(.id != $id))[]' <<<"$queue" >"$tmp"
        mandate_write_if_changed "$tmp" "$MANDATE_QUEUE_FILE"
        rm -f "$tmp"
        trap - EXIT
        printf '%s\n' "$row"
        ;;
      defer)
        jq -c --arg id "$id" '
          map(if .id == $id then .state = "deferred" else . end)[]' <<<"$queue" >"$tmp"
        mandate_write_if_changed "$tmp" "$MANDATE_QUEUE_FILE"
        rm -f "$tmp"
        trap - EXIT
        jq -c '.state = "deferred"' <<<"$row"
        ;;
    esac
    ;;
  *)
    echo "usage: queue.sh list | lint | approve <id> | reject <id> | defer <id>" >&2
    exit 2
    ;;
esac
