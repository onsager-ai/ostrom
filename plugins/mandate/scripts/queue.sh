#!/usr/bin/env bash
# Mutate the private file-backed queue only after an explicit /desk decision.

set -euo pipefail
umask 077

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=mandate-lib.sh
source "$SCRIPT_DIR/mandate-lib.sh"

command -v jq >/dev/null 2>&1 || { echo "mandate queue: jq is required" >&2; exit 1; }

action="${1:-list}"
id="${2:-}"
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
    echo "usage: queue.sh list | approve <id> | reject <id> | defer <id>" >&2
    exit 2
    ;;
esac
