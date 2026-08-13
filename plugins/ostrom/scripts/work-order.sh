#!/usr/bin/env bash
# Create and validate durable implementation work orders.
#
# Durable contract (schema_version 1): a work order is one canonical JSON file
# per item under work-orders/. Its exact fields are validated here so a future
# Rust dispatcher can consume Bash-era orders without guessing at an ad-hoc
# shape. The filename is <item_hash>.json, where item_hash is sha256(item_id);
# rewriting it creates a new order_id while preserving the one-file-per-item
# contract. New orders derive branch_name from item_id. Validation deliberately
# continues to accept every historically valid schema_version 1 branch_name so
# orders already on disk can recover through implementer's retarget path.

set -euo pipefail
umask 077

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=mandate-lib.sh
source "$SCRIPT_DIR/mandate-lib.sh"

command -v jq >/dev/null 2>&1 || {
  echo "ostrom work order: jq is required" >&2
  exit 1
}
command -v sha256sum >/dev/null 2>&1 || {
  echo "ostrom work order: sha256sum is required" >&2
  exit 1
}

DEFAULT_COST_CEILING_USD=20
DEFAULT_TOKEN_CEILING=500000

usage() {
  echo "usage: work-order.sh create <candidate-json-file> | validate <work-order-file> | item-hash <item-id> | branch-name <item-id>" >&2
  exit 2
}

candidate_schema='
  type == "object"
  and (keys == ["acceptance_criteria", "branch_name", "constraints", "item_id", "item_ref", "repository", "schema_version", "spec"])
  and .schema_version == 1
  and (.item_id | type == "string" and length > 0)
  and (.repository | type == "string" and test("^[^/[:space:]]+/[^/[:space:]]+$"))
  and (.item_ref | type == "string" and length > 0)
  and (.branch_name | type == "string" and test("^[A-Za-z0-9][A-Za-z0-9._/-]*$") and (contains("..") | not))
  and (.spec | type == "string" and length > 0)
  and (.acceptance_criteria | type == "array" and length > 0 and all(.[]; type == "string" and length > 0))
  and (.constraints | type == "array" and all(.[]; type == "string" and length > 0))
'

order_schema='
  type == "object"
  and (keys == ["acceptance_criteria", "branch_name", "constraints", "cost_ceiling_usd", "created_at", "item_id", "item_ref", "order_id", "repository", "schema_version", "spec", "token_ceiling"])
  and .schema_version == 1
  and (.order_id | type == "string" and test("^[0-9a-f]{64}$"))
  and (.item_id | type == "string" and length > 0)
  and (.repository | type == "string" and test("^[^/[:space:]]+/[^/[:space:]]+$"))
  and (.item_ref | type == "string" and length > 0)
  and (.branch_name | type == "string" and test("^[A-Za-z0-9][A-Za-z0-9._/-]*$") and (contains("..") | not))
  and (.spec | type == "string" and length > 0)
  and (.acceptance_criteria | type == "array" and length > 0 and all(.[]; type == "string" and length > 0))
  and (.constraints | type == "array" and all(.[]; type == "string" and length > 0))
  and (.created_at | type == "string" and test("^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z$"))
  and (.cost_ceiling_usd | type == "number" and . > 0)
  and (.token_ceiling | type == "number" and . > 0 and . == floor)
'

item_hash() {
  printf '%s' "$1" | sha256sum | awk '{print $1}'
}

branch_name() {
  local item_id="$1"
  local hash number
  hash="$(item_hash "$item_id")"
  number="${item_id##*#}"
  case "$number" in
    ''|*[!0-9]*) printf 'ostrom/item-%s\n' "${hash:0:20}" ;;
    *) printf 'ostrom/%s-%s\n' "$number" "${hash:0:12}" ;;
  esac
}

validate_order() {
  order_file="$1"
  [ -f "$order_file" ] || {
    echo "ostrom work order: $order_file is not a file" >&2
    return 2
  }
  if ! jq -e "$order_schema" "$order_file" >/dev/null 2>&1; then
    echo "ostrom work order: invalid schema_version 1 work order at $order_file" >&2
    return 2
  fi
}

case "${1:-}" in
  create)
    [ "$#" -eq 2 ] || usage
    candidate="$2"
    [ -f "$candidate" ] || {
      echo "ostrom work order: candidate is not a file" >&2
      exit 2
    }
    if ! jq -e "$candidate_schema" "$candidate" >/dev/null 2>&1; then
      echo "ostrom work order: candidate does not match schema_version 1" >&2
      exit 2
    fi

    cost_ceiling="${MANDATE_ORDER_COST_CEILING_USD:-$DEFAULT_COST_CEILING_USD}"
    token_ceiling="${MANDATE_ORDER_TOKEN_CEILING:-$DEFAULT_TOKEN_CEILING}"
    if ! jq -en --arg value "$cost_ceiling" '$value | tonumber | . > 0' >/dev/null 2>&1; then
      echo "ostrom work order: cost ceiling must be a positive number" >&2
      exit 2
    fi
    if ! jq -en --arg value "$token_ceiling" '$value | tonumber | . > 0 and . == floor' >/dev/null 2>&1; then
      echo "ostrom work order: token ceiling must be a positive integer" >&2
      exit 2
    fi

    item_id="$(jq -r '.item_id' "$candidate")"
    item_hash="$(item_hash "$item_id")"
    deterministic_branch="$(branch_name "$item_id")"
    supplied_branch="$(jq -r '.branch_name' "$candidate")"
    if [ "$supplied_branch" != "$deterministic_branch" ]; then
      echo "ostrom work order: overwriting candidate branch_name '$supplied_branch' with item-derived '$deterministic_branch'" >&2
    fi
    created_at="${MANDATE_TRACE_TIME:-$(date -u +%Y-%m-%dT%H:%M:%SZ)}"
    order_id="$(printf '%s\n%s\n%s\n' "$item_id" "$created_at" "$RANDOM" | sha256sum | awk '{print $1}')"
    orders_dir="$MANDATE_DATA_DIR/work-orders"
    target="$orders_dir/$item_hash.json"
    mkdir -p "$orders_dir"

    # A running implementer reads this deterministic per-item path. Never
    # replace the artifact beneath it: dispatch acquires the same
    # item_hash-derived lease before launch and retains it until the terminal
    # row is durable.
    implementer_lease="$MANDATE_DATA_DIR/implementer-item-$item_hash.lease"
    if [ -e "$implementer_lease" ]; then
      echo "ostrom work order: item has a live implementer lease; refusing to replace $target" >&2
      exit 3
    fi
    if [ -f "$target" ] && [ -s "$MANDATE_DATA_DIR/sprint.jsonl" ]; then
      previous_order_id="$(jq -r '.order_id' "$target" 2>/dev/null)" || previous_order_id=""
      if [ -n "$previous_order_id" ] && jq -Rn -e --arg order_id "$previous_order_id" '
        [inputs | try fromjson catch null | select(type == "object")] as $rows
        | any($rows[];
            .kind == "work-dispatched" and .fact.order_id == $order_id)
          and (any($rows[];
            (.kind == "work-completed" or .kind == "work-failed")
            and .fact.order_id == $order_id) | not)
      ' "$MANDATE_DATA_DIR/sprint.jsonl" >/dev/null; then
        echo "ostrom work order: prior order is still in flight; refusing to replace $target" >&2
        exit 3
      fi
    fi

    tmp="$(mktemp "$orders_dir/.order.XXXXXX")"
    trap 'rm -f "$tmp"' EXIT
    jq -c \
      --arg order_id "$order_id" \
      --arg created_at "$created_at" \
      --arg branch_name "$deterministic_branch" \
      --argjson cost_ceiling_usd "$cost_ceiling" \
      --argjson token_ceiling "$token_ceiling" \
      '. + {
        order_id: $order_id,
        created_at: $created_at,
        branch_name: $branch_name,
        cost_ceiling_usd: $cost_ceiling_usd,
        token_ceiling: $token_ceiling
      }' "$candidate" >"$tmp"
    validate_order "$tmp"
    mv "$tmp" "$target"
    trap - EXIT
    printf '%s\n' "$target"
    ;;
  validate)
    [ "$#" -eq 2 ] || usage
    validate_order "$2"
    ;;
  item-hash)
    [ "$#" -eq 2 ] || usage
    [ -n "$2" ] || usage
    item_hash "$2"
    ;;
  branch-name)
    [ "$#" -eq 2 ] || usage
    [ -n "$2" ] || usage
    branch_name "$2"
    ;;
  *)
    usage
    ;;
esac
