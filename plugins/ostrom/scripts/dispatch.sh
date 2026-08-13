#!/usr/bin/env bash
# Dispatch a durable work order through a selectable backend.
#
# `dispatch` is the protocol verb; systemd is only its first backend. Triage
# calls this script without knowing where the implementer will run. A hosted
# backend can therefore replace dispatch_systemd without changing work orders
# or the triage protocol.

set -euo pipefail
umask 077

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=mandate-lib.sh
source "$SCRIPT_DIR/mandate-lib.sh"

command -v jq >/dev/null 2>&1 || { echo "ostrom dispatch: jq is required" >&2; exit 1; }

DEFAULT_DAILY_CAP_USD=50
DEFAULT_MAX_IMPLEMENTERS=2
IMPLEMENTER_LEASE_TTL_SECONDS="${MANDATE_IMPLEMENTER_LEASE_TTL_SECONDS:-2592000}"
TRACE_FILE="$MANDATE_DATA_DIR/sprint.jsonl"
GH_AS_BIN="${MANDATE_GH_AS_BIN:-$SCRIPT_DIR/gh-as.sh}"
SYSTEMD_RUN_BIN="${MANDATE_SYSTEMD_RUN_BIN:-systemd-run}"
IMPLEMENTER_BIN="${MANDATE_IMPLEMENTER_BIN:-$SCRIPT_DIR/implement.sh}"
CODEX_COMMAND="${CODEX_BIN:-codex}"
backend="${MANDATE_DISPATCH_BACKEND:-systemd}"

usage() {
  echo "usage: dispatch.sh <work-order-file>" >&2
  exit 2
}

[ "$#" -eq 1 ] || usage
order_file="$1"
bash "$SCRIPT_DIR/work-order.sh" validate "$order_file"

item_id="$(jq -r '.item_id' "$order_file")"
repository="$(jq -r '.repository' "$order_file")"
item_ref="$(jq -r '.item_ref' "$order_file")"
order_id="$(jq -r '.order_id' "$order_file")"
cost_ceiling_usd="$(jq -r '.cost_ceiling_usd' "$order_file")"
token_ceiling="$(jq -r '.token_ceiling' "$order_file")"
branch_name="$(jq -r '.branch_name' "$order_file")"
item_hash="$(bash "$SCRIPT_DIR/work-order.sh" item-hash "$item_id")"
unit_name="ostrom-implementer-${item_hash:0:16}"

append_dispatch_failure() {
  local reason="$1"
  local duration_seconds="${2:-0}"
  local worktree_path="${3:-}"
  local preserved_branch="${4:-}"
  local failed_repository="${5:-}"
  local head_sha="${6:-}"
  local ahead_of_default="${7:-}"
  local failed_fact
  failed_fact="$(jq -cn \
    --arg item_id "$item_id" --arg order_id "$order_id" \
    --arg unit_name "$unit_name" --arg backend "$backend" \
    --argjson cost_ceiling_usd "$cost_ceiling_usd" \
    --argjson token_ceiling "$token_ceiling" \
    --argjson duration_seconds "$duration_seconds" \
    --arg reason "$reason" --arg worktree_path "$worktree_path" \
    --arg branch_name "$preserved_branch" \
    --arg repository "$failed_repository" --arg head_sha "$head_sha" \
    --arg ahead_of_default "$ahead_of_default" \
    '{schema_version: 1, item_id: $item_id, order_id: $order_id,
      unit_name: $unit_name, backend: $backend,
      cost_ceiling_usd: $cost_ceiling_usd, token_ceiling: $token_ceiling,
      cost_usd: 0, duration_seconds: $duration_seconds,
      pr_url: null, reason: $reason,
      worktree_path: (if $worktree_path == "" then null else $worktree_path end),
      branch_name: (if $branch_name == "" then null else $branch_name end),
      repository: (if $repository == "" then null else $repository end),
      head_sha: (if $head_sha == "" then null else $head_sha end),
      ahead_of_default: (if $ahead_of_default == "" then null
        elif $ahead_of_default == "unknown" then "unknown"
        else ($ahead_of_default | tonumber) end),
      usage: {input_tokens: 0, cached_input_tokens: 0,
        output_tokens: 0, reasoning_output_tokens: 0}}')"
  bash "$SCRIPT_DIR/trace.sh" append work-failed "$failed_fact" '{}' >/dev/null
}

local_default_ref() {
  local worktree="$1"
  local ref candidate count
  ref="$(git -C "$worktree" symbolic-ref -q refs/remotes/origin/HEAD 2>/dev/null)" || ref=""
  if [ -n "$ref" ] && git -C "$worktree" rev-parse --verify "$ref^{commit}" >/dev/null 2>&1; then
    printf '%s\n' "$ref"
    return 0
  fi
  for candidate in refs/remotes/origin/main refs/remotes/origin/master; do
    if git -C "$worktree" rev-parse --verify "$candidate^{commit}" >/dev/null 2>&1; then
      printf '%s\n' "$candidate"
      return 0
    fi
  done
  ref="$(git -C "$worktree" for-each-ref --format='%(refname)' refs/remotes/origin \
    | grep -v '/HEAD$' || true)"
  count="$(printf '%s\n' "$ref" | sed '/^$/d' | wc -l | tr -d '[:space:]')"
  if [ "$count" -eq 1 ]; then
    printf '%s\n' "$ref"
    return 0
  fi
  return 1
}

# A mismatching worktree that contains preserved work cannot be satisfied by
# the implementer. Reject it before reserving the item lease, a concurrency
# slot, or the order's cost ceiling. The implementer repeats this check after
# fetch, under the lease, to close the preflight race and retarget clean trees.
worktree_root="$MANDATE_DATA_DIR/implementer-worktrees/$item_hash"
if [ -e "$worktree_root" ]; then
  existing_branch="$(git -C "$worktree_root" branch --show-current 2>/dev/null)" || existing_branch=""
  if [ -n "$existing_branch" ] && [ "$existing_branch" != "$branch_name" ]; then
    worktree_status="$(git -C "$worktree_root" status --porcelain 2>/dev/null)" || worktree_status=unreadable
    default_ref="$(local_default_ref "$worktree_root")" || default_ref=""
    ahead_count=unknown
    if [ -n "$default_ref" ]; then
      ahead_count="$(git -C "$worktree_root" rev-list --count "$default_ref..HEAD" 2>/dev/null)" || ahead_count=unknown
    fi
    if [ -n "$worktree_status" ] || [ "$ahead_count" = unknown ] || [ "$ahead_count" -gt 0 ]; then
      append_dispatch_failure worktree-branch-mismatch 0 "$worktree_root" "$existing_branch" ||
        echo "ostrom dispatch: could not record work-failed" >&2
      echo "ostrom dispatch: worktree branch mismatch preserves work at $worktree_root on $existing_branch (order expects $branch_name)" >&2
      exit 3
    fi
  fi
fi

# A pushed branch is durable work even when no pull request references the
# item yet. Enumerate every remote branch through the builder credential and
# reject matching work before resolving the backend, acquiring the item lease,
# or calculating either concurrency or spend reservations.
remote_branch_pages="$({
  bash "$GH_AS_BIN" builder "$repository" \
    gh api --paginate --slurp "repos/$repository/branches?per_page=100"
} 2>/dev/null)" || {
  echo "ostrom dispatch: could not verify remote branches for $item_id in $repository" >&2
  exit 1
}
if ! jq -e '
  type == "array"
  and all(.[]; type == "array")
  and all(.[][];
    type == "object"
    and (.name | type == "string")
    and (.commit.sha | type == "string" and test("^[0-9a-fA-F]{40}$")))
' >/dev/null 2>&1 <<<"$remote_branch_pages"; then
  echo "ostrom dispatch: could not verify remote branches for $item_id in $repository: invalid response" >&2
  exit 1
fi

item_number="${item_id##*#}"
case "$item_number" in *[!0-9]*|'') item_number="" ;; esac
matching_remote_branch="$(jq -r \
  --arg expected "$branch_name" --arg number "$item_number" '
  [ .[][]
    | select(.name == $expected
        or ($number != "" and (.name | test("(^|/)" + $number + "-"))))
  ]
  | sort_by(if .name == $expected then 0 else 1 end)
  | first // empty
  | [.name, .commit.sha]
  | @tsv
' <<<"$remote_branch_pages")"
if [ -n "$matching_remote_branch" ]; then
  IFS=$'\t' read -r pushed_branch pushed_head_sha <<<"$matching_remote_branch"
  ahead_of_default=unknown
  default_branch="$({
    bash "$GH_AS_BIN" builder "$repository" \
      gh repo view "$repository" --json defaultBranchRef \
        --jq '.defaultBranchRef.name'
  } 2>/dev/null)" || default_branch=""
  if [ -n "$default_branch" ]; then
    default_head_sha="$(jq -r --arg branch "$default_branch" '
      first(.[][] | select(.name == $branch) | .commit.sha) // empty
    ' <<<"$remote_branch_pages")"
    if [ -n "$default_head_sha" ]; then
      compared_ahead="$({
        bash "$GH_AS_BIN" builder "$repository" \
          gh api "repos/$repository/compare/$default_head_sha...$pushed_head_sha" \
            --jq '.ahead_by'
      } 2>/dev/null)" || compared_ahead=""
      case "$compared_ahead" in
        ''|*[!0-9]*) ;;
        *) ahead_of_default="$compared_ahead" ;;
      esac
    fi
  fi
  append_dispatch_failure branch-already-pushed 0 "" "$pushed_branch" \
    "$repository" "$pushed_head_sha" "$ahead_of_default" ||
      echo "ostrom dispatch: could not record work-failed" >&2
  echo "ostrom dispatch: remote work already exists: repository=$repository branch=$pushed_branch head=$pushed_head_sha ahead=$ahead_of_default" >&2
  exit 3
fi

codex_unavailable() {
  local detail="$1"
  append_dispatch_failure codex-unavailable ||
    echo "ostrom dispatch: could not record work-failed" >&2
  echo "ostrom dispatch: Codex is unavailable: $detail" >&2
  exit 1
}

absolute_executable() {
  local candidate="$1"
  local candidate_dir candidate_name
  [ -x "$candidate" ] || return 1
  case "$candidate" in
    /*) printf '%s\n' "$candidate" ;;
    *)
      candidate_dir="$(dirname "$candidate")"
      candidate_name="$(basename "$candidate")"
      candidate_dir="$(cd -P "$candidate_dir" 2>/dev/null && pwd)" || return 1
      printf '%s/%s\n' "$candidate_dir" "$candidate_name"
      ;;
  esac
}

resolve_codex() {
  local candidate nvm_dir
  candidate="$(command -v "$CODEX_COMMAND" 2>/dev/null)" || candidate=""
  if [ -n "$candidate" ]; then
    absolute_executable "$candidate" && return 0
  fi

  # An explicit path is authoritative. Bare command names also search nvm's
  # installed versions because non-interactive sessions do not load nvm.
  case "$CODEX_COMMAND" in */*) return 1 ;; esac
  nvm_dir="${NVM_DIR:-$HOME/.nvm}"
  while IFS= read -r candidate; do
    absolute_executable "$candidate" && return 0
  done < <(printf '%s\n' "$nvm_dir"/versions/node/*/bin/"$CODEX_COMMAND" | LC_ALL=C sort -Vr)
  return 1
}

resolved_codex_bin="$(resolve_codex)" ||
  codex_unavailable "$CODEX_COMMAND was not found"
resolved_node_bin="$(bash "$SCRIPT_DIR/run-node.sh" --resolve-only 2>/dev/null)" ||
  codex_unavailable "Node.js could not be resolved for $resolved_codex_bin"
node_bin_dir="$(dirname "$resolved_node_bin")"
inherited_path="${PATH:-/usr/local/bin:/usr/bin:/bin}"
unit_path="$node_bin_dir:$inherited_path"

# Exercise the resolved executable with exactly the PATH the transient unit
# receives. This catches an env shebang whose interpreter exists for neither
# the user manager nor the accepted Codex path before reserving a dispatch.
if ! PATH="$unit_path" "$resolved_codex_bin" --version >/dev/null 2>&1; then
  codex_unavailable "$resolved_codex_bin cannot execute with resolved Node $resolved_node_bin"
fi

# Lease naming is a durable interoperability contract: every implementation
# derives `implementer-item-<item_hash>.lease`, where item_hash is lowercase
# sha256(item_id). It is per item, never per role or dispatch attempt, so Bash
# and Rust dispatchers exclude one another safely during cutover.
lease_name="implementer-item-$item_hash.lease"
lease_owner="$unit_name"
lease_acquired=0

release_dispatch_lease() {
  if [ "$lease_acquired" -eq 1 ]; then
    MANDATE_LEASE_NAME="$lease_name" \
      bash "$SCRIPT_DIR/lease.sh" release "$lease_owner" >/dev/null 2>&1 || true
    lease_acquired=0
  fi
}
trap release_dispatch_lease EXIT
trap 'release_dispatch_lease; exit 129' HUP
trap 'release_dispatch_lease; exit 130' INT
trap 'release_dispatch_lease; exit 143' TERM

inflight_rows() {
  [ -s "$TRACE_FILE" ] || { printf '[]\n'; return; }
  jq -Rn '
    [inputs | try fromjson catch null | select(type == "object")] as $rows
    | [
        $rows[]
        | select(.kind == "work-dispatched" and (.fact.order_id | type) == "string")
        | .fact as $dispatch
        | select(any($rows[];
            (.kind == "work-completed" or .kind == "work-failed")
            and .fact.order_id == $dispatch.order_id
          ) | not)
        | $dispatch
      ]
  ' "$TRACE_FILE"
}

if MANDATE_LEASE_NAME="$lease_name" \
  bash "$SCRIPT_DIR/lease.sh" acquire "$lease_owner" "$IMPLEMENTER_LEASE_TTL_SECONDS" >/dev/null 2>&1; then
  lease_acquired=1
else
  lease_status=$?
  if [ "$lease_status" -eq 3 ]; then
    echo "ostrom dispatch: item already has a live implementer lease: $item_id" >&2
  else
    echo "ostrom dispatch: could not acquire implementer lease for $item_id (rc=$lease_status)" >&2
  fi
  exit "$lease_status"
fi

open_prs="$({
  bash "$GH_AS_BIN" builder "$repository" \
    gh pr list --repo "$repository" --state open --limit 1000 \
      --json number,title,body,url
} 2>/dev/null)" || {
  echo "ostrom dispatch: could not verify open pull requests for $item_id" >&2
  exit 1
}
if jq -e --arg item "$item_id" --arg ref "$item_ref" '
  any(.[];
    (((.title // "") + "\n" + (.body // "")) | contains($item))
    or (((.title // "") + "\n" + (.body // ""))
      | test("(?im)(^|[[:space:]])(Close[sd]?|Fix(e[sd])?|Refs?|References)[[:space:]]+" + ($ref | gsub("([\\[\\]().*+?^$|{}])"; "\\\\\\1")) + "([[:space:][:punct:]]|$)"))
  )
' >/dev/null <<<"$open_prs"; then
  echo "ostrom dispatch: an open pull request already references $item_id" >&2
  exit 3
fi

inflight="$(inflight_rows)"
if jq -e --arg item "$item_id" 'any(.[]; .item_id == $item)' >/dev/null <<<"$inflight"; then
  echo "ostrom dispatch: an earlier work-dispatched row has no terminal row for $item_id" >&2
  exit 3
fi

max_implementers="${MANDATE_MAX_IMPLEMENTERS:-$DEFAULT_MAX_IMPLEMENTERS}"
case "$max_implementers" in
  ''|*[!0-9]*|0)
    echo "ostrom dispatch: MANDATE_MAX_IMPLEMENTERS must be a positive integer" >&2
    exit 2
    ;;
esac
inflight_count="$(jq 'length' <<<"$inflight")"
if [ "$inflight_count" -ge "$max_implementers" ]; then
  echo "ostrom dispatch: concurrency limit reached ($inflight_count/$max_implementers)" >&2
  exit 3
fi

daily_cap_usd="${MANDATE_DAILY_CAP_USD:-$DEFAULT_DAILY_CAP_USD}"
if ! jq -en --arg value "$daily_cap_usd" '$value | tonumber | . > 0' >/dev/null 2>&1; then
  daily_cap_usd="$DEFAULT_DAILY_CAP_USD"
fi
now_epoch="${MANDATE_NOW_EPOCH:-$(date +%s)}"
case "$now_epoch" in ''|*[!0-9]*) now_epoch="$(date +%s)" ;; esac
today="$(date -u -d "@$now_epoch" +%Y-%m-%d)"
actual_spend=0
if [ -s "$TRACE_FILE" ]; then
  actual_spend="$(jq -Rn --arg day "$today" '
    reduce inputs as $line
      (0;
        ($line | try fromjson catch null) as $event
        | if (($event | type) == "object")
            and (($event.ts // "") | startswith($day))
            and ($event.kind == "pass-ended" or $event.kind == "work-completed" or $event.kind == "work-failed")
            and (($event.fact.cost_usd? | type) == "number")
          then . + $event.fact.cost_usd
          else .
          end)
  ' "$TRACE_FILE")"
fi
reserved_spend="$(jq '[.[].cost_ceiling_usd | select(type == "number")] | add // 0' <<<"$inflight")"
projected_spend="$(jq -n \
  --argjson actual "$actual_spend" \
  --argjson reserved "$reserved_spend" \
  --argjson order "$cost_ceiling_usd" \
  '$actual + $reserved + $order')"
if [ "$(jq -n --argjson projected "$projected_spend" --argjson cap "$daily_cap_usd" '$projected > $cap')" = true ]; then
  echo "ostrom dispatch: daily spend cap would be exceeded by this order ($projected_spend > $daily_cap_usd USD)" >&2
  exit 3
fi

case "$backend" in
  systemd) ;;
  *)
    echo "ostrom dispatch: unsupported backend: $backend" >&2
    exit 2
    ;;
esac

# Trace contract schema_version 1. All work lifecycle facts use the same
# stable identifiers and units. work-dispatched has schema_version, item_id,
# order_id, unit_name, backend, both ceilings, cost_usd:null, and
# duration_seconds:0. Terminal rows repeat those fields with numeric cost_usd
# and measured duration_seconds, then add pr_url, reason, and token usage.
# Rust readers may rely on these names and meanings.
dispatch_fact="$(jq -cn \
  --arg item_id "$item_id" --arg order_id "$order_id" \
  --arg unit_name "$unit_name" --arg backend "$backend" \
  --argjson cost_ceiling_usd "$cost_ceiling_usd" \
  --argjson token_ceiling "$token_ceiling" \
  '{schema_version: 1, item_id: $item_id, order_id: $order_id,
    unit_name: $unit_name, backend: $backend,
    cost_ceiling_usd: $cost_ceiling_usd, token_ceiling: $token_ceiling,
    cost_usd: null, duration_seconds: 0}')"
if ! bash "$SCRIPT_DIR/trace.sh" append work-dispatched "$dispatch_fact" '{}' >/dev/null; then
  echo "ostrom dispatch: could not record work-dispatched" >&2
  exit 1
fi
dispatch_started="$(date +%s)"
trap '' HUP INT TERM
if ! "$SYSTEMD_RUN_BIN" --user \
  --unit "$unit_name" \
  --description "Ostrom implementer $item_id" \
  --collect --no-block \
  --property RuntimeMaxSec=infinity \
  --property KillMode=control-group \
  --setenv "CLAUDE_CONFIG_DIR=$MANDATE_CONFIG_DIR" \
  --setenv "CLAUDE_PLUGIN_ROOT=$MANDATE_PLUGIN_ROOT" \
  --setenv "MANDATE_DAILY_CAP_USD=$daily_cap_usd" \
  --setenv "MANDATE_MAX_IMPLEMENTERS=$max_implementers" \
  --setenv "MANDATE_DISPATCH_BACKEND=$backend" \
  --setenv "CODEX_BIN=$resolved_codex_bin" \
  --setenv "PATH=$unit_path" \
  "$IMPLEMENTER_BIN" "$order_file" "$unit_name"; then
  trap 'release_dispatch_lease; exit 129' HUP
  trap 'release_dispatch_lease; exit 130' INT
  trap 'release_dispatch_lease; exit 143' TERM
  dispatch_duration=$(( $(date +%s) - dispatch_started ))
  [ "$dispatch_duration" -ge 0 ] || dispatch_duration=0
  append_dispatch_failure dispatch-failed "$dispatch_duration" || true
  echo "ostrom dispatch: systemd backend failed to launch $unit_name" >&2
  exit 1
fi

# Ownership passes to the transient unit. Its EXIT/signal trap writes the
# terminal trace row and releases this exact per-item lease.
lease_acquired=0
trap - EXIT HUP INT TERM
printf '%s\n' "$unit_name"
