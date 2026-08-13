#!/usr/bin/env bash
# Append and read the machine-local sprint trace.
#
# A builder or gatekeeper consuming another builder's trace must use `read`,
# which structurally omits narration. `read-narration` is an explicit,
# principal-facing path: acting on another builder's narration would preserve
# the contamination path and merely move it through the filesystem.

set -euo pipefail
umask 077

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=mandate-lib.sh
source "$SCRIPT_DIR/mandate-lib.sh"

command -v jq >/dev/null 2>&1 || { echo "mandate trace: jq is required" >&2; exit 1; }

TRACE_FILE="$MANDATE_DATA_DIR/sprint.jsonl"
TRACE_MAX_BYTES=4096

usage() {
  echo "usage: trace.sh append <kind> <fact-json> <narration-json> | read | read-narration" >&2
  exit 2
}

validate_trace='
  type == "object"
  and (keys == ["fact", "kind", "narration", "ts"])
  and (.ts | type == "string" and length > 0)
  and (.kind | type == "string" and length > 0)
  and (.fact | type == "object")
  and (.narration | type == "object")
'

action="${1:-}"

case "$action" in
  append)
    [ "$#" -eq 4 ] || usage
    kind="$2"
    fact_json="$3"
    narration_json="$4"
    [ -n "$kind" ] || { echo "mandate trace: kind must not be empty" >&2; exit 2; }

    if ! jq -e 'type == "object"' >/dev/null 2>&1 <<<"$fact_json"; then
      echo "mandate trace: fact-json must be a JSON object" >&2
      exit 2
    fi
    if ! jq -e 'type == "object"' >/dev/null 2>&1 <<<"$narration_json"; then
      echo "mandate trace: narration-json must be a JSON object" >&2
      exit 2
    fi

    # Cutover 2026-08-13: historical item-worked rows may carry the stable
    # item_hash in order_id and remain readable, but new rows must use an
    # order_id taken from the contents of a durable work-order file. The two
    # hashes are unrelated, so accepting an unresolvable value would make
    # cross-kind trace joins silently meaningless.
    if [ "$kind" = "item-worked" ] && jq -e 'has("order_id")' >/dev/null <<<"$fact_json"; then
      if ! jq -e '.order_id | type == "string" and length > 0' >/dev/null <<<"$fact_json"; then
        echo "mandate trace: item-worked order_id must be a non-empty string from a work order's order_id field" >&2
        exit 2
      fi
      order_id="$(jq -r '.order_id' <<<"$fact_json")"
      order_id_found=0
      for order_file in "$MANDATE_DATA_DIR"/work-orders/*.json; do
        [ -f "$order_file" ] || continue
        if jq -e --arg order_id "$order_id" \
          'type == "object" and .order_id == $order_id' \
          "$order_file" >/dev/null 2>&1; then
          order_id_found=1
          break
        fi
      done
      if [ "$order_id_found" -ne 1 ]; then
        echo "mandate trace: item-worked order_id '$order_id' matches no work order's order_id field" >&2
        exit 2
      fi
    fi

    record="$(
      jq -cn \
        --arg ts "${MANDATE_TRACE_TIME:-$(date -u +%Y-%m-%dT%H:%M:%SZ)}" \
        --arg kind "$kind" \
        --argjson fact "$fact_json" \
        --argjson narration "$narration_json" \
        '{ts: $ts, kind: $kind, fact: $fact, narration: $narration}'
    )"
    record_bytes="$(LC_ALL=C printf '%s\n' "$record" | wc -c | tr -d '[:space:]')"
    if [ "$record_bytes" -gt "$TRACE_MAX_BYTES" ]; then
      echo "mandate trace: record is $record_bytes bytes; maximum is $TRACE_MAX_BYTES" >&2
      exit 2
    fi

    mkdir -p "$MANDATE_DATA_DIR"
    # Match queue JSONL discipline: one small printf call means one write(2)
    # under O_APPEND. Oversized records are rejected rather than risking an
    # interleaved line on filesystems where PIPE_BUF is the useful bound.
    printf '%s\n' "$record" >>"$TRACE_FILE"
    printf '%s\n' "$record"
    ;;
  read)
    [ "$#" -eq 1 ] || usage
    [ -s "$TRACE_FILE" ] || exit 0
    jq -c "if $validate_trace then {ts, kind, fact} else error(\"malformed sprint trace record\") end" \
      "$TRACE_FILE"
    ;;
  read-narration)
    [ "$#" -eq 1 ] || usage
    [ -s "$TRACE_FILE" ] || exit 0
    jq -c "if $validate_trace then {ts, kind, narration} else error(\"malformed sprint trace record\") end" \
      "$TRACE_FILE"
    ;;
  *)
    usage
    ;;
esac
