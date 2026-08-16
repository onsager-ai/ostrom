#!/usr/bin/env bash
# Order delegated queue rows without changing the mandate boundary.

set -euo pipefail
umask 077

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=mandate-lib.sh
source "$SCRIPT_DIR/mandate-lib.sh"

command -v jq >/dev/null 2>&1 || { echo "mandate selection: jq is required" >&2; exit 1; }

usage() {
  echo "usage: select-work.sh list | select <owner> [already-attempted-id ...]" >&2
  exit 2
}

action="${1:-}"
case "$action" in
  list) [ "$#" -eq 1 ] || usage ;;
  select)
    [ "$#" -ge 2 ] || usage
    [ -n "$2" ] || { echo "mandate selection: owner must not be empty" >&2; exit 2; }
    ;;
  *) usage ;;
esac

config="$(mandate_load_config)" || exit
queue="$(mandate_read_queue)" || exit

if [ ! -s "$MANDATE_STATE_FILE" ]; then
  echo "mandate selection: dependency graph has no sweep state; run sweep before selecting work" >&2
  exit 4
fi
state="$(jq -c 'if type == "object" then . else error("state is not an object") end' "$MANDATE_STATE_FILE")" || {
  echo "mandate selection: cannot read $MANDATE_STATE_FILE" >&2
  exit 4
}

# Linux caps a single argv entry at MAX_ARG_STRLEN (128KiB), independently of
# the much larger ARG_MAX total. A real state.json is already ~900KiB, so
# passing it through --argjson fails with "Argument list too long" — and
# because that failure is a jq exec error rather than a queue fault, selection
# returned zero rows and exit 0, which reads as a quiet portfolio.
#
# Only the node map is ever needed downstream, and it is read from a file so
# neither it nor the state can reintroduce the limit as the portfolio grows.
graph_file="$(mktemp)"
trap 'rm -f "$graph_file"' EXIT
jq -c '.dependency_graph.nodes | map({key: .id, value: .}) | from_entries' \
  <<<"$state" >"$graph_file"
if ! jq -e --argjson queue "$queue" --argjson config "$config" '
    .dependency_graph as $graph
    | $graph.graph_version == 1
    and $graph.configured_repositories == ($config.projects | map(.repo) | sort)
    and ($graph.nodes | type == "array")
    and ($graph.edges | type == "array")
    and ($graph.faults | type == "array")
    and (([$queue[].id] - [$graph.nodes[].id]) | length) == 0
    and all($queue[];
      . as $row
      | (($row.blocked_by // []) | sort) == ([$graph.edges[]
          | select(.item == $row.id and (.sources | index("body") != null))
          | .dependency] | sort)
    )
  ' >/dev/null <<<"$state"; then
  echo "mandate selection: dependency graph is absent, stale, or invalid; run sweep before selecting work" >&2
  exit 4
fi
jq -r '
  .dependency_graph.faults[]?
  | "mandate selection: dependency graph fault \(.name): \(.nodes | join(", "))"
' <<<"$state" >&2

# The sweep copies the ranking and any verified stale pointers into state.
# Refuse an active ranking that has not passed through that sweep: otherwise a
# config edit could steer one pass before its stale references were checked.
ranking_count="$(jq '.work_ranking | length' <<<"$config")"
if [ "$ranking_count" -gt 0 ]; then
  if [ ! -s "$MANDATE_STATE_FILE" ]; then
    echo "mandate selection: active work_ranking has no sweep state; run sweep before selecting work" >&2
    exit 4
  fi
  if ! jq -e --argjson ranking "$(jq '.work_ranking' <<<"$config")" \
      '.work_ranking == $ranking' >/dev/null <<<"$state"; then
    echo "mandate selection: active work_ranking differs from the last sweep; run sweep before selecting work" >&2
    exit 4
  fi
  ranking_faults="$(jq -c '.work_ranking_faults // []' <<<"$state")"
  if [ "$(jq 'length' <<<"$ranking_faults")" -gt 0 ]; then
    jq -r '.[] | "mandate selection: stale work_ranking item \(.) no longer exists"' \
      <<<"$ranking_faults" >&2
    exit 4
  fi
fi

candidates_and_gated="$(jq -cn --argjson queue "$queue" --slurpfile graph_rows "$graph_file" '
  def authorized:
    .kind != "parked"
    and .state != "deferred"
    and (
      (.kind | IN("moved", "stuck"))
      or (
        .state == "approved"
        and (.kind | IN("tripwire", "decision"))
      )
    );
  ($graph_rows[0]) as $graph
  | [$queue[] | select(authorized) | . + {graph: $graph[.id]}] as $authorized
  | {
      candidates: [$authorized[] | select(.graph.dispatchable) | del(.graph)],
      gated: [$authorized[] | select(.graph.dispatchable | not)]
    }
')"
candidates="$(jq -c '.candidates' <<<"$candidates_and_gated")"

# A plan is advisory and usable only for the exact queue/authorization facts
# it observed. A stale or malformed plan cannot steer selection; the existing
# work_ranking/dependency/age order remains the mechanical fallback.
plan_order='[]'
has_plan=false
plan_status="absent"
plan_rejection_clause=""
if [ -s "$MANDATE_PLAN_FILE" ]; then
  queue_basis="$(jq -cn --argjson queue "$queue" --slurpfile graph_rows "$graph_file" '
    ($graph_rows[0]) as $graph
    | [
      $queue[] | {
        id,
        opened,
        kind,
        state,
        blocked_by: (.blocked_by // []),
        graph_dispatchable: ($graph[.id].dispatchable // false),
        unblocking_power: ($graph[.id].unblocking_power // 0)
      }
    ]')"
  if jq -e \
      --argjson basis "$queue_basis" \
      --argjson ranking "$(jq '.work_ranking' <<<"$config")" \
      --argjson candidates "$candidates" '
        .plan_version == 1
        and .queue_basis == $basis
        and .ranking.work_ranking == $ranking
        and (.ranking.ordered | type == "array")
        and (.ranking.ordered | length) == (.ranking.ordered | unique | length)
        and (.ranking.ordered | sort) == ($candidates | map(.id) | sort)
      ' "$MANDATE_PLAN_FILE" >/dev/null 2>&1; then
    plan_order="$(jq -c '.ranking.ordered' "$MANDATE_PLAN_FILE")"
    has_plan=true
    plan_status="applied"
  else
    plan_status="rejected"
    # Diagnose the already-rejected plan without feeding this result back into
    # has_plan or plan_order. Clause order deliberately mirrors the guard.
    plan_rejection_clause="$(
      jq -r \
        --argjson basis "$queue_basis" \
        --argjson ranking "$(jq '.work_ranking' <<<"$config")" \
        --argjson candidates "$candidates" '
          if ((try (.plan_version == 1) catch false) | not) then
            "plan_version"
          elif ((try (.queue_basis == $basis) catch false) | not) then
            "queue_basis"
          elif ((try (.ranking.work_ranking == $ranking) catch false) | not) then
            "work_ranking"
          elif ((try (.ranking.ordered | type == "array") catch false) | not) then
            "ordered_not_array"
          elif ((.ranking.ordered | length) != (.ranking.ordered | unique | length)) then
            "ordered_duplicates"
          elif ((.ranking.ordered | sort) != ($candidates | map(.id) | sort)) then
            "candidate_set_mismatch"
          else
            "predicate_error"
          end
        ' "$MANDATE_PLAN_FILE" 2>/dev/null
    )" || plan_rejection_clause="malformed_json"
    echo "mandate selection: stale or invalid plan.json ignored; using mechanical ranking" >&2
  fi
fi

ordered="$(
  jq -cn \
    --argjson config "$config" \
    --argjson queue "$queue" \
    --argjson candidates "$candidates" \
    --slurpfile graph_rows "$graph_file" \
    --argjson plan_order "$plan_order" \
    --argjson has_plan "$has_plan" '
      ($graph_rows[0]) as $graph
      | ($config.work_ranking) as $ranking
      | $candidates
          | map(
              . as $candidate
              | ($ranking | index($candidate.id)) as $rank
              | ($plan_order | index($candidate.id)) as $plan_rank
              | ($graph[$candidate.id].unblocking_power // 0) as $unblocks
              | {
                  row: $candidate,
                  key: (
                    if $rank != null then [0, $rank, 0, $candidate.opened, $candidate.id]
                    elif $has_plan and $plan_rank != null then [1, $plan_rank, 0, $candidate.opened, $candidate.id]
                    else [2, 0, -$unblocks, $candidate.opened, $candidate.id]
                    end
                  )
                }
            )
          | sort_by(.key)
          | map(.row)
    '
)" || exit

if [ "$action" = "list" ]; then
  jq -c '.[]' <<<"$ordered"
  exit 0
fi

owner="$2"
shift 2
attempted="$(printf '%s\n' "$@" | jq -Rn '[inputs | select(length > 0)]')"
remaining="$(jq -c --argjson attempted "$attempted" '[.[] | . as $row | select(($attempted | index($row.id)) == null)]' <<<"$ordered")"
selected="$(jq -c 'first // empty' <<<"$remaining")"
if [ -z "$selected" ]; then
  exit 3
fi

# Record a graph gate only when it changed this selection: the oldest
# authorization-valid, not-yet-attempted item was structurally or temporally
# gated and a different item was selected in its place.
authorized_age_first="$(jq -c --argjson attempted "$attempted" '
  [(.candidates + .gated)[]
    | . as $row
    | select(($attempted | index($row.id)) == null)]
  | sort_by(.opened, .id)
  | first // empty
' <<<"$candidates_and_gated")"
if [ -n "$authorized_age_first" ] \
    && [ "$(jq -r '.id' <<<"$authorized_age_first")" != "$(jq -r '.id' <<<"$selected")" ] \
    && [ "$(jq -r 'has("graph")' <<<"$authorized_age_first")" = true ]; then
  graph_gated_id="$(jq -r '.id' <<<"$authorized_age_first")"
  graph_gate_fact="$(jq -cn \
    --arg owner "$owner" \
    --arg selected "$(jq -r '.id' <<<"$selected")" \
    --arg gated "$graph_gated_id" \
    --argjson node "$(jq -c '.graph' <<<"$authorized_age_first")" \
    --argjson cycle "$(jq --arg id "$graph_gated_id" '
      any(.dependency_graph.faults[]?; .name == "dependency_cycle" and (.nodes | index($id) != null))
    ' <<<"$state")" '
      {
        owner: $owner,
        action: "dependency-graph-gate",
        selected: $selected,
        gated: $gated,
        unsatisfied: $node.unsatisfied,
        children: $node.children,
        cycle: $cycle
      }
    ')"
  bash "$SCRIPT_DIR/trace.sh" append work-graph-gated "$graph_gate_fact" '{}' >/dev/null
fi

# Compare the chosen row with the exact legacy order over the same remaining
# dispatchable set. Every departure gets a fact-only trace naming its cause.
age_first="$(jq -c --argjson attempted "$attempted" '
  [.[] | . as $row | select(($attempted | index($row.id)) == null)]
  | sort_by(.opened, .id)
  | first // empty
' <<<"$candidates")"

if [ -n "$age_first" ] && [ "$(jq -r '.id' <<<"$selected")" != "$(jq -r '.id' <<<"$age_first")" ]; then
  selected_id="$(jq -r '.id' <<<"$selected")"
  selected_repo="$(jq -r '.repo' <<<"$selected")"
  selected_ref="$(jq -r '.ref' <<<"$selected")"
  displaced_id="$(jq -r '.id' <<<"$age_first")"
  ranking_position="$(jq --arg id "$selected_id" '(.work_ranking | index($id)) // -1' <<<"$config")"
  if [ "$ranking_position" -ge 0 ]; then
    ranking_name="work_ranking"
    ranking_position=$((ranking_position + 1))
  elif [ "$has_plan" = true ] && [ "$(jq --arg id "$selected_id" 'index($id) != null' <<<"$plan_order")" = true ]; then
    ranking_name="goal-plan"
    ranking_position="$(jq --arg id "$selected_id" 'index($id) + 1' <<<"$plan_order")"
  else
    ranking_name="dependency-unblocks"
    ranking_position=0
  fi
  trace_fact="$(jq -cn \
    --arg owner "$owner" \
    --arg repo "$selected_repo" \
    --arg ref "$selected_ref" \
    --arg selected "$selected_id" \
    --arg displaced "$displaced_id" \
    --arg ranking "$ranking_name" \
    --argjson ranking_position "$ranking_position" '
      {
        owner: $owner,
        repo: $repo,
        ref: $ref,
        action: "delegated-selection",
        selected: $selected,
        displaced: $displaced,
        ranking: $ranking
      }
      + if $ranking_position > 0
        then {ranking_position: $ranking_position}
        else {}
        end
    ')"
  bash "$SCRIPT_DIR/trace.sh" append work-ranked "$trace_fact" '{}' >/dev/null
fi

plan_trace_fact="$(jq -cn \
  --arg owner "$owner" \
  --arg repo "$(jq -r '.repo' <<<"$selected")" \
  --arg ref "$(jq -r '.ref' <<<"$selected")" \
  --arg selected "$(jq -r '.id' <<<"$selected")" \
  --arg plan_status "$plan_status" \
  --arg plan_rejection_clause "$plan_rejection_clause" '
    {
      owner: $owner,
      repo: $repo,
      ref: $ref,
      action: "delegated-selection",
      selected: $selected,
      plan_status: $plan_status
    }
    + if $plan_status == "rejected"
      then {plan_rejection_clause: $plan_rejection_clause}
      else {}
      end
  ')"
bash "$SCRIPT_DIR/trace.sh" append plan-selection "$plan_trace_fact" '{}' >/dev/null

printf '%s\n' "$selected"
