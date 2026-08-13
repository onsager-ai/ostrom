#!/usr/bin/env bash
# Execute one durable work order in a dedicated worktree.
#
# This process is intentionally not wall-clock bounded. systemd owns its
# lifecycle and journal; the order's dollar reservation and weighted-token
# ceiling bound spend instead. Codex edits offline inside workspace-write.
# Authentication, fetch, commit, push, and PR creation happen in this wrapper,
# outside the Codex sandbox.

set -uo pipefail
umask 077

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=mandate-lib.sh
source "$SCRIPT_DIR/mandate-lib.sh"

usage() {
  echo "usage: implement.sh <work-order-file> <unit-name>" >&2
  exit 2
}

[ "$#" -eq 2 ] || usage
order_file="$1"
unit_name="$2"
bash "$SCRIPT_DIR/work-order.sh" validate "$order_file" || exit

item_id="$(jq -r '.item_id' "$order_file")"
repository="$(jq -r '.repository' "$order_file")"
item_ref="$(jq -r '.item_ref' "$order_file")"
branch_name="$(jq -r '.branch_name' "$order_file")"
order_id="$(jq -r '.order_id' "$order_file")"
cost_ceiling_usd="$(jq -r '.cost_ceiling_usd' "$order_file")"
token_ceiling="$(jq -r '.token_ceiling' "$order_file")"
item_hash="$(bash "$SCRIPT_DIR/work-order.sh" item-hash "$item_id")"

# Durable lease-name contract; see dispatch.sh. The transient unit owns this
# per-item lease from dispatch until this process records a terminal row.
lease_name="implementer-item-$item_hash.lease"
lease_owner="$unit_name"
CODEX_BIN="${CODEX_BIN:-codex}"
GH_AS_BIN="${MANDATE_GH_AS_BIN:-$SCRIPT_DIR/gh-as.sh}"
dispatch_backend="${MANDATE_DISPATCH_BACKEND:-systemd}"
start_epoch="$(date +%s)"
child_pid=""
monitor_pid=""
terminal_written=0
failure_reason="implementer-exited"
pr_url=""
events_file=""
events_pipe=""
streaming_ceiling_marker=""
worktree_root=""
streaming_ceiling_mode="${MANDATE_IMPLEMENTER_STREAMING_CEILING:-enabled}"

usage_fact() {
  if [ -n "$events_file" ] && [ -s "$events_file" ]; then
    jq -Rn '
      reduce inputs as $line
        ({input_tokens: 0, cached_input_tokens: 0, output_tokens: 0,
          reasoning_output_tokens: 0, cached_input_tokens_available: true,
          completed_turns: 0};
          ($line | try fromjson catch null) as $event
          | if (($event | type) == "object") and $event.type == "turn.completed"
              and (($event.usage | type) == "object")
            then .completed_turns += 1
              | .input_tokens += ($event.usage.input_tokens // 0)
              | if (($event.usage.cached_input_tokens? | type) == "number")
                then .cached_input_tokens += $event.usage.cached_input_tokens
                else .cached_input_tokens_available = false
                end
              | .output_tokens += ($event.usage.output_tokens // 0)
              | .reasoning_output_tokens += ($event.usage.reasoning_output_tokens // 0)
            else . end)
      | if .completed_turns == 0 or (.cached_input_tokens_available | not)
        then .fresh_input_tokens = null
          | .cached_input_tokens = null
          | .cached_input_tokens_available = false
        else .fresh_input_tokens = ([.input_tokens - .cached_input_tokens, 0] | max)
        end
      | del(.completed_turns)
    ' "$events_file" 2>/dev/null || printf '%s\n' \
      '{"input_tokens":0,"fresh_input_tokens":null,"cached_input_tokens":null,"output_tokens":0,"reasoning_output_tokens":0,"cached_input_tokens_available":false}'
  else
    printf '%s\n' \
      '{"input_tokens":0,"fresh_input_tokens":null,"cached_input_tokens":null,"output_tokens":0,"reasoning_output_tokens":0,"cached_input_tokens_available":false}'
  fi
}

weighted_token_count() {
  # OpenAI prices GPT-5-Codex cached input at $0.125/M versus $1.25/M
  # fresh input (1:10):
  # https://developers.openai.com/api/docs/models/gpt-5-codex
  # Apply that ratio to Ostrom's 0.2 fresh weight, giving cached input 0.02.
  # If the harness omits the cached count, use the explicit upper bound that
  # all input was fresh; usage_fact records the component split as unknown.
  jq '
    (if .cached_input_tokens_available
      then ((.fresh_input_tokens // 0) * 0.2
        + (.cached_input_tokens // 0) * 0.02)
      else ((.input_tokens // 0) * 0.2)
      end
      + (.output_tokens // 0)) | ceil
  '
}

monitor_codex_events() {
  monitored_pid="$1"
  while IFS= read -r event_line || [ -n "$event_line" ]; do
    printf '%s\n' "$event_line" >>"$events_file"
    if [ "$streaming_ceiling_mode" = disabled ] || \
      [ -s "$streaming_ceiling_marker" ] || \
      ! jq -e '
        type == "object" and .type == "turn.completed"
        and (.usage | type == "object")
      ' >/dev/null 2>&1 <<<"$event_line"; then
      continue
    fi
    streamed_usage="$(usage_fact)"
    streamed_weighted_tokens="$(weighted_token_count <<<"$streamed_usage")"
    if [ "$streamed_weighted_tokens" -gt "$token_ceiling" ] && \
      kill -TERM "$monitored_pid" 2>/dev/null; then
      printf '%s\n' "$streamed_weighted_tokens" >"$streaming_ceiling_marker"
    fi
  done
}

preserved_work_fact() {
  preserved_path=""
  preserved_branch=""
  if [ -n "$worktree_root" ] && [ -e "$worktree_root" ]; then
    worktree_status="$(git -C "$worktree_root" status --porcelain 2>/dev/null || true)"
    ahead_count=0
    if [ -n "${default_branch:-}" ]; then
      ahead_count="$(git -C "$worktree_root" rev-list --count \
        "refs/remotes/origin/$default_branch..HEAD" 2>/dev/null || printf '0\n')"
    fi
    if [ -n "$worktree_status" ] || [ "$ahead_count" -gt 0 ]; then
      preserved_path="$worktree_root"
      preserved_branch="$(git -C "$worktree_root" branch --show-current \
        2>/dev/null || true)"
      [ -n "$preserved_branch" ] || preserved_branch="$branch_name"
    fi
  fi
  jq -cn --arg worktree_path "$preserved_path" \
    --arg branch_name "$preserved_branch" \
    '{worktree_path: (if $worktree_path == "" then null else $worktree_path end),
      branch_name: (if $branch_name == "" then null else $branch_name end)}'
}

append_terminal() {
  kind="$1"
  reason="$2"
  end_epoch="$(date +%s)"
  duration_seconds=$((end_epoch - start_epoch))
  [ "$duration_seconds" -ge 0 ] || duration_seconds=0
  usage="$(usage_fact)"
  weighted_tokens="$(weighted_token_count <<<"$usage")"
  # schema_version 1 cost_usd is Ostrom's normalized order-cost estimate:
  # weighted_tokens / token_ceiling * cost_ceiling_usd. It is numeric on
  # terminal rows (including zero before the first completed turn), never a
  # claim about a provider invoice. The same conversion makes completed work
  # count against the daily cap after its in-flight reservation is released.
  cost_usd="$(jq -n \
    --argjson weighted_tokens "$weighted_tokens" \
    --argjson token_ceiling "$token_ceiling" \
    --argjson cost_ceiling_usd "$cost_ceiling_usd" \
    '($weighted_tokens / $token_ceiling * $cost_ceiling_usd)')"
  preserved_work='{"worktree_path":null,"branch_name":null}'
  if [ "$kind" = work-failed ]; then
    preserved_work="$(preserved_work_fact)"
  fi
  terminal_fact="$(jq -cn \
    --arg item_id "$item_id" --arg order_id "$order_id" \
    --arg unit_name "$unit_name" --arg backend "$dispatch_backend" \
    --argjson cost_ceiling_usd "$cost_ceiling_usd" \
    --argjson token_ceiling "$token_ceiling" \
    --argjson weighted_tokens "$weighted_tokens" \
    --argjson cost_usd "$cost_usd" \
    --argjson duration_seconds "$duration_seconds" \
    --arg pr_url "$pr_url" --arg reason "$reason" \
    --argjson preserved_work "$preserved_work" \
    --argjson usage "$usage" \
    '{schema_version: 1, item_id: $item_id, order_id: $order_id,
      unit_name: $unit_name, backend: $backend,
      cost_ceiling_usd: $cost_ceiling_usd, token_ceiling: $token_ceiling,
      weighted_tokens: $weighted_tokens, cost_usd: $cost_usd,
      duration_seconds: $duration_seconds,
      pr_url: (if $pr_url == "" then null else $pr_url end),
      reason: (if $reason == "" then null else $reason end),
      worktree_path: $preserved_work.worktree_path,
      branch_name: $preserved_work.branch_name,
      usage: $usage}')"
  if bash "$SCRIPT_DIR/trace.sh" append "$kind" "$terminal_fact" '{}' >/dev/null; then
    terminal_written=1
    return 0
  fi
  echo "ostrom implementer: could not append $kind for $item_id" >&2
  return 1
}

finish() {
  saved_status=$?
  trap - EXIT HUP INT TERM
  if [ -n "$child_pid" ] && kill -0 "$child_pid" 2>/dev/null; then
    kill -TERM "$child_pid" 2>/dev/null || true
    wait "$child_pid" 2>/dev/null || true
    child_pid=""
  fi
  if [ -n "$monitor_pid" ]; then
    if kill -0 "$monitor_pid" 2>/dev/null; then
      kill -TERM "$monitor_pid" 2>/dev/null || true
    fi
    wait "$monitor_pid" 2>/dev/null || true
    monitor_pid=""
  fi
  if [ -n "$events_pipe" ]; then
    rm -f "$events_pipe"
    events_pipe=""
  fi
  if [ "$terminal_written" -eq 0 ]; then
    append_terminal work-failed "$failure_reason" || {
      [ "$saved_status" -ne 0 ] || saved_status=1
    }
  fi
  if ! MANDATE_LEASE_NAME="$lease_name" \
    bash "$SCRIPT_DIR/lease.sh" release "$lease_owner" >/dev/null 2>&1; then
    echo "ostrom implementer: could not release $lease_name" >&2
    [ "$saved_status" -ne 0 ] || saved_status=1
  fi
  exit "$saved_status"
}

on_signal() {
  signal_name="$1"
  signal_status="$2"
  failure_reason="signal-${signal_name}"
  exit "$signal_status"
}

trap finish EXIT
trap 'on_signal HUP 129' HUP
trap 'on_signal INT 130' INT
trap 'on_signal TERM 143' TERM

lease_json="$(MANDATE_LEASE_NAME="$lease_name" bash "$SCRIPT_DIR/lease.sh" status 2>/dev/null)" || {
  failure_reason=lease-missing
  exit 1
}
if [ "$(jq -r '.owner' <<<"$lease_json")" != "$lease_owner" ]; then
  failure_reason=lease-owner-mismatch
  exit 1
fi

find_source_repository() {
  if [ -n "${MANDATE_IMPLEMENTER_SOURCE_REPO:-}" ]; then
    [ -d "$MANDATE_IMPLEMENTER_SOURCE_REPO" ] || return 1
    printf '%s\n' "$MANDATE_IMPLEMENTER_SOURCE_REPO"
    return
  fi
  config="$(mandate_load_config)" || return
  while IFS= read -r root; do
    [ -d "$root" ] || continue
    while IFS= read -r marker; do
      candidate="${marker%/.git}"
      remote="$(git -C "$candidate" remote get-url origin 2>/dev/null)" || continue
      normalized="${remote%.git}"
      normalized="${normalized#https://github.com/}"
      normalized="${normalized#git@github.com:}"
      if [ "$normalized" = "$repository" ]; then
        printf '%s\n' "$candidate"
        return 0
      fi
    done < <(find "$root" -name .git -print -prune 2>/dev/null)
  done < <(jq -r '.search_roots[]' <<<"$config")
  return 1
}

source_repository="$(find_source_repository)" || {
  failure_reason=source-repository-not-found
  exit 1
}

if ! command -v "$CODEX_BIN" >/dev/null 2>&1; then
  failure_reason=codex-unavailable
  exit 1
fi

default_branch="$({
  bash "$GH_AS_BIN" builder "$repository" \
    gh repo view "$repository" --json defaultBranchRef --jq '.defaultBranchRef.name'
} 2>/dev/null)" || {
  failure_reason=default-branch-query-failed
  exit 1
}
[ -n "$default_branch" ] || {
  failure_reason=default-branch-missing
  exit 1
}

if ! bash "$GH_AS_BIN" builder "$repository" \
  git -C "$source_repository" fetch \
    "https://github.com/$repository.git" \
    "$default_branch:refs/remotes/origin/$default_branch"; then
  failure_reason=fetch-failed
  exit 1
fi

worktrees_dir="$MANDATE_DATA_DIR/implementer-worktrees"
worktree_root="$worktrees_dir/$item_hash"
mkdir -p "$worktrees_dir"
if [ -e "$worktree_root" ]; then
  existing_branch="$(git -C "$worktree_root" branch --show-current 2>/dev/null)" || {
    failure_reason=worktree-unreadable
    exit 1
  }
  if [ "$existing_branch" != "$branch_name" ]; then
    worktree_status="$(git -C "$worktree_root" status --porcelain 2>/dev/null)" || {
      failure_reason=worktree-unreadable
      exit 1
    }
    ahead_count="$(git -C "$worktree_root" rev-list --count \
      "refs/remotes/origin/$default_branch..HEAD" 2>/dev/null)" || {
      failure_reason=worktree-unreadable
      exit 1
    }
    if [ -n "$worktree_status" ] || [ "$ahead_count" -gt 0 ]; then
      failure_reason=worktree-branch-mismatch
      exit 1
    fi
    if git -C "$worktree_root" show-ref --verify --quiet \
      "refs/heads/$branch_name"; then
      if ! git -C "$worktree_root" switch "$branch_name"; then
        failure_reason=worktree-retarget-failed
        exit 1
      fi
    elif ! git -C "$worktree_root" switch -c "$branch_name" \
      "refs/remotes/origin/$default_branch"; then
      failure_reason=worktree-retarget-failed
      exit 1
    fi
  fi
else
  if ! git -C "$source_repository" worktree add -b "$branch_name" \
    "$worktree_root" "refs/remotes/origin/$default_branch"; then
    failure_reason=worktree-create-failed
    exit 1
  fi
fi
preexisting_commits="$(git -C "$worktree_root" rev-list --count \
  "refs/remotes/origin/$default_branch..HEAD" 2>/dev/null)" || preexisting_commits=0

runs_dir="$MANDATE_DATA_DIR/implementer-runs/$order_id"
mkdir -p "$runs_dir"
prompt_file="$runs_dir/prompt.md"
result_file="$runs_dir/result.md"
events_file="$runs_dir/events.jsonl"
events_pipe="$runs_dir/events.pipe"
streaming_ceiling_marker="$runs_dir/token-ceiling-terminated"
jq -r '
  "Implement this work order. Work only in the current worktree. Do not commit, push, open a pull request, or use the network; the outer harness owns those steps. Run proportionate tests. Do not redesign the agreed spec.\n\n"
  + "Item: " + .item_id + "\nBranch: " + .branch_name + "\n"
  + "Cost ceiling: $" + (.cost_ceiling_usd | tostring)
  + "; weighted-token ceiling: " + (.token_ceiling | tostring) + "\n\n"
  + "Spec:\n" + .spec + "\n\nAcceptance criteria:\n"
  + (.acceptance_criteria | map("- " + .) | join("\n"))
  + "\n\nConstraints:\n" + (.constraints | map("- " + .) | join("\n"))
' "$order_file" >"$prompt_file"

# Codex exec is non-interactive; keep its never-approve policy explicit in
# configuration. workspace-write permits the diff but keeps network off; the
# wrapper performs every authenticated/network mutation after Codex exits.
# The pinned CLI cannot parse rollout_budget configuration, so supervise its
# JSON event stream and terminate the child as soon as completed-turn usage
# crosses the weighted-token ceiling. The post-run check remains a backstop.
rm -f "$events_pipe" "$streaming_ceiling_marker"
: >"$events_file"
if ! mkfifo "$events_pipe"; then
  failure_reason=event-stream-create-failed
  exit 1
fi
"$CODEX_BIN" exec --json \
  -C "$worktree_root" \
  -s workspace-write \
  -c approval_policy=\"never\" \
  -c sandbox_workspace_write.network_access=false \
  -c web_search=\"disabled\" \
  -o "$result_file" \
  <"$prompt_file" >"$events_pipe" 2>&1 &
child_pid=$!
monitor_codex_events "$child_pid" <"$events_pipe" &
monitor_pid=$!
wait "$child_pid"
codex_status=$?
child_pid=""
wait "$monitor_pid" 2>/dev/null || true
monitor_pid=""
rm -f "$events_pipe"
events_pipe=""
if [ -s "$streaming_ceiling_marker" ]; then
  failure_reason=token-ceiling-terminated
  exit 1
fi
if [ "$codex_status" -ne 0 ]; then
  case "$codex_status" in
    126|127) failure_reason=codex-unavailable ;;
    1)
      if grep -Eq \
        '^Error loading config\.toml:|^Error: features\.[[:alnum:]_.]+ is required when [[:alnum:]_.]+ is enabled$' \
        "$events_file"; then
        failure_reason=codex-invocation-invalid
      else
        failure_reason=codex-exit-1
      fi
      ;;
    2)
      if grep -q '^Usage: codex exec ' "$events_file"; then
        failure_reason=codex-invocation-invalid
      else
        failure_reason=codex-exit-2
      fi
      ;;
    *) failure_reason="codex-exit-$codex_status" ;;
  esac
  exit "$codex_status"
fi

usage="$(usage_fact)"
weighted_tokens="$(weighted_token_count <<<"$usage")"
if [ "$weighted_tokens" -gt "$token_ceiling" ]; then
  failure_reason=token-ceiling-exceeded
  exit 1
fi

if git -C "$worktree_root" diff --quiet && \
  git -C "$worktree_root" diff --cached --quiet && \
  [ -z "$(git -C "$worktree_root" ls-files --others --exclude-standard)" ] && \
  [ "$preexisting_commits" -eq 0 ]; then
  failure_reason=no-changes
  exit 1
fi
if ! git -C "$worktree_root" diff --quiet || \
  ! git -C "$worktree_root" diff --cached --quiet || \
  [ -n "$(git -C "$worktree_root" ls-files --others --exclude-standard)" ]; then
  if ! git -C "$worktree_root" add -A; then
    failure_reason=stage-failed
    exit 1
  fi
  if ! git -C "$worktree_root" commit \
    -m "feat: implement $item_ref" \
    -m "Ostrom-Role: builder"; then
    failure_reason=commit-failed
    exit 1
  fi
fi
if ! bash "$GH_AS_BIN" builder "$repository" \
  git -C "$worktree_root" push \
    "https://github.com/$repository.git" "HEAD:refs/heads/$branch_name"; then
  failure_reason=push-failed
  exit 1
fi

body_file="$runs_dir/pr-body.md"
jq -r '
  "Closes " + .item_id + "\n\n"
  + "## Work order\n\n" + .spec + "\n\n"
  + "## Acceptance criteria\n\n"
  + (.acceptance_criteria | map("- " + .) | join("\n")) + "\n\n"
  + "## Implementation harness\n\n"
  + "Codex ran non-interactively with `workspace-write`, approval policy `never`, and network disabled. The outer implementer wrapper performed fetch, commit, push, and pull-request creation outside the Codex sandbox.\n\n"
  + "The order reserved $" + (.cost_ceiling_usd | tostring)
  + " and enforced a " + (.token_ceiling | tostring) + " weighted-token ceiling.\n\n"
  + "Ostrom-Role: builder\n"
' "$order_file" >"$body_file"
pr_title="Implement $item_id"
pr_url="$({
  bash "$GH_AS_BIN" builder "$repository" \
    gh pr create --repo "$repository" --base "$default_branch" \
      --head "$branch_name" --title "$pr_title" --body-file "$body_file"
} 2>/dev/null)" || {
  failure_reason=pr-create-failed
  pr_url=""
  exit 1
}

if ! append_terminal work-completed ""; then
  failure_reason=terminal-trace-failed
  exit 1
fi
if ! MANDATE_LEASE_NAME="$lease_name" \
  bash "$SCRIPT_DIR/lease.sh" release "$lease_owner" >/dev/null; then
  failure_reason=lease-release-failed
  exit 1
fi
trap - EXIT HUP INT TERM
printf '%s\n' "$pr_url"
