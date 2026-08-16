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

# The sweep copies the ranking and any verified stale pointers into state.
# Refuse an active ranking that has not passed through that sweep: otherwise a
# config edit could steer one pass before its stale references were checked.
ranking_count="$(jq '.work_ranking | length' <<<"$config")"
if [ "$ranking_count" -gt 0 ]; then
  if [ ! -s "$MANDATE_STATE_FILE" ]; then
    echo "mandate selection: active work_ranking has no sweep state; run sweep before selecting work" >&2
    exit 4
  fi
  state="$(jq -c 'if type == "object" then . else error("state is not an object") end' "$MANDATE_STATE_FILE")" || {
    echo "mandate selection: cannot read $MANDATE_STATE_FILE" >&2
    exit 4
  }
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

candidates="$(jq -cn --argjson queue "$queue" '
  def dispatchable:
    .kind != "parked"
    and .state != "deferred"
    and (
      (.kind | IN("moved", "stuck"))
      or (
        .state == "approved"
        and (.kind | IN("tripwire", "decision"))
      )
    );
  [$queue[] | select(dispatchable)]
')"

# A plan is advisory and usable only for the exact queue/authorization facts
# it observed. A stale or malformed plan cannot steer selection; the existing
# work_ranking/dependency/age order remains the mechanical fallback.
plan_order='[]'
has_plan=false
if [ -s "$MANDATE_PLAN_FILE" ]; then
  queue_basis="$(jq -cn --argjson queue "$queue" '[
    $queue[] | {
      id,
      opened,
      kind,
      state,
      blocked_by: (.blocked_by // [])
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
  else
    echo "mandate selection: stale or invalid plan.json ignored; using mechanical ranking" >&2
  fi
fi

ordered="$(
  jq -cn \
    --argjson config "$config" \
    --argjson queue "$queue" \
    --argjson candidates "$candidates" \
    --argjson plan_order "$plan_order" \
    --argjson has_plan "$has_plan" '
      ($config.work_ranking) as $ranking
      | if (($ranking | length) == 0 and ($has_plan | not)) then
          $candidates | sort_by(.opened, .id)
        else
          $candidates
          | map(
              . as $candidate
              | ($ranking | index($candidate.id)) as $rank
              | ($plan_order | index($candidate.id)) as $plan_rank
              | ([
                  $queue[]
                  | select(.state != "deferred" and .kind != "parked")
                  | select((.blocked_by // []) | index($candidate.id) != null)
                ] | length) as $unblocks
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
        end
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

printf '%s\n' "$selected"
