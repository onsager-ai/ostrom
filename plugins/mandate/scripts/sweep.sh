#!/usr/bin/env bash
# Read-only GitHub portfolio sweep. The only writes are private queue/state.

set -euo pipefail
umask 077

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=mandate-lib.sh
source "$SCRIPT_DIR/mandate-lib.sh"

command -v jq >/dev/null 2>&1 || { echo "mandate sweep: jq is required" >&2; exit 1; }
command -v gh >/dev/null 2>&1 || { echo "mandate sweep: gh is required" >&2; exit 1; }

if ! mandate_is_configured; then
  echo "mandate sweep: no mandates.yaml found at $MANDATE_USER_CONFIG or $MANDATE_REPO_CONFIG" >&2
  exit 2
fi

config="$(mandate_load_config)" || exit
project_count="$(jq '.projects | length' <<<"$config")"
if [ "$project_count" -eq 0 ]; then
  echo "mandate sweep: mandates.yaml contains no projects" >&2
  exit 2
fi

gh_host="${GH_HOST:-github.com}"
if ! gh auth status --hostname "$gh_host" >/dev/null 2>&1; then
  echo "mandate sweep: gh is not authenticated for $gh_host; run 'gh auth login'" >&2
  exit 3
fi

mkdir -p "$MANDATE_DATA_DIR"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

existing_queue="$(mandate_read_queue)" || {
  echo "mandate sweep: cannot read $MANDATE_QUEUE_FILE" >&2
  exit 4
}
if [ -s "$MANDATE_STATE_FILE" ]; then
  if ! old_state="$(jq -c 'if type == "object" then . else error("state is not an object") end' "$MANDATE_STATE_FILE")"; then
    echo "mandate sweep: cannot read $MANDATE_STATE_FILE" >&2
    exit 4
  fi
else
  old_state='{"version":1,"repos":{}}'
fi

sweep_started="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
generated='[]'
active_ids='[]'
new_state="$old_state"

while IFS= read -r project; do
  repo="$(jq -r '.repo' <<<"$project")"
  paused="$(jq -r '.paused' <<<"$project")"
  policy="$(
    jq -cn \
      --argjson project "$project" \
      --argjson bounce_all "$(jq -c '.bounce_all' <<<"$config")" \
      '{
        delegated: $project.delegated,
        paused: $project.paused,
        bounce: $project.bounce,
        bounce_all: $bounce_all
      }'
  )"
  gh_error="$work/gh-error"

  if [ "$paused" = "true" ]; then
    issues='[]'
  else
    if ! issues="$(gh issue list --repo "$repo" --state open --limit 100 \
      --json number,title,labels,createdAt,updatedAt,url 2>"$gh_error")"; then
      detail="$(tr '\n' ' ' <"$gh_error")"
      echo "mandate sweep: failed to query open issues for $repo${detail:+: $detail}" >&2
      exit 5
    fi
  fi
  if ! prs="$(gh pr list --repo "$repo" --state open --limit 100 \
    --json number,title,labels,createdAt,updatedAt,url,isDraft,reviewDecision,statusCheckRollup 2>"$gh_error")"; then
    detail="$(tr '\n' ' ' <"$gh_error")"
    echo "mandate sweep: failed to query open PRs and CI for $repo${detail:+: $detail}" >&2
    exit 5
  fi

  items="$(
    jq -cn --arg repo "$repo" --argjson issues "$issues" --argjson prs "$prs" '
      def failure:
        ((.conclusion // .state // "") | ascii_upcase)
        | IN("FAILURE", "ERROR", "CANCELLED", "TIMED_OUT", "ACTION_REQUIRED", "STALE");
      def success:
        ((.conclusion // .state // "") | ascii_upcase)
        | IN("SUCCESS", "NEUTRAL", "SKIPPED");
      def ci_state:
        (.statusCheckRollup // []) as $checks
        | if any($checks[]?; failure) then "failing"
          elif ($checks | length) > 0 and all($checks[]; success) then "passing"
          else "pending"
          end;
      def normalized($type):
        . as $item
        | (if $type == "pr" then ($item | ci_state) else "none" end) as $ci
        | {
            id: ($repo + "#" + (.number | tostring)),
            repo: $repo,
            ref: ("#" + (.number | tostring)),
            type: $type,
            title: .title,
            labels: [(.labels // [])[] | .name],
            opened: .createdAt,
            updated: .updatedAt,
            ci: $ci,
            ready: ($type == "pr" and (.isDraft | not) and $ci == "passing"),
            review: (.reviewDecision // "")
          }
        | .fingerprint = ([
            .title,
            (.labels | sort | join(",")),
            .ci,
            (.ready | tostring),
            .review
          ] | join("|"));
      [($issues[] | normalized("issue")), ($prs[] | normalized("pr"))]
    '
  )"

  old_repo_state="$(jq -c --arg repo "$repo" '.repos[$repo] // {}' <<<"$old_state")"
  rows="$(
    jq -cn \
      --arg sweep_started "$sweep_started" \
      --argjson project "$project" \
      --argjson config "$config" \
      --argjson items "$items" \
      --argjson policy "$policy" \
      --argjson previous "$old_repo_state" '
      def tripwire_hit($haystack; $shared; $local):
        first(
          (($shared[]? | {source: "bounce_all", term: .}),
           ($local[]? | {source: "project bounce", term: .}))
          | select(
              (.term | ascii_downcase) as $needle
              | ($haystack | ascii_downcase | contains($needle))
            )
        );
      def inside_delegated_scope($haystack; $delegated):
        ($delegated | ascii_downcase) as $scope
        | ($haystack | ascii_downcase | contains($scope));
      def mandate_record($item; $kind; $reason; $hit):
        if $kind == "tripwire" then
          {
            reason: $reason,
            dossier: {
              question: ("May " + $item.repo + $item.ref + " cross the matched mandate tripwire?"),
              options_ruled_out: [
                "Auto-proceed — a tripwire requires human judgment."
              ],
              recommended_action: ("Review " + $item.repo + $item.ref + ", then approve, reject, or defer it in /desk."),
              blast_radius: ($item.repo + $item.ref + " only.")
            }
          }
        else {reason: $reason}
        end;
      [
        $items[]
        | . as $item
        | ($previous.items[$item.id] // null) as $old
        | select(
            ($previous.cursor // null) == null
            or ($previous.policy // null) != $policy
            or $old == null
            or $old.fingerprint != $item.fingerprint
            or $item.updated > $previous.cursor
          )
        | if $project.paused then
            select(.type == "pr" and .ci == "failing")
            | {
                id: .id,
                repo: .repo,
                ref: .ref,
                kind: "drift",
                mandate: {reason: "paused project CI is failing"},
                state: "pending",
                opened: .opened
              }
          else
            (([.title] + .labels) | join(" ")) as $haystack
            | (tripwire_hit($haystack; $config.bounce_all; $project.bounce) // null) as $hit
            | (inside_delegated_scope($haystack; $project.delegated)) as $inside
            | (
                if $hit != null then
                  {kind: "tripwire", reason: ($hit.source + ": " + $hit.term)}
                elif ($inside | not) then
                  {
                    kind: "decision",
                    reason: ("out-of-mandate: outside delegated outcome \"" + $project.delegated + "\"")
                  }
                elif .type == "pr" and .ci == "failing" then
                  {kind: "drift", reason: "CI is failing"}
                elif (($sweep_started | fromdateiso8601) - (.updated | fromdateiso8601)) >= ($config.stuck_after_days * 86400) then
                  {kind: "stuck", reason: ("no movement for " + ($config.stuck_after_days | tostring) + " days")}
                elif .ready then
                  {kind: "decision", reason: "open PR passed CI"}
                else
                  {kind: "moved", reason: "updated since the read cursor"}
                end
              ) as $classification
            | {
                id: .id,
                repo: .repo,
                ref: .ref,
                kind: $classification.kind,
                mandate: mandate_record($item; $classification.kind; $classification.reason; $hit),
                state: "pending",
                opened: .opened
              }
          end
      ]
    '
  )"

  generated="$(jq -cn --argjson all "$generated" --argjson rows "$rows" '$all + $rows')"
  if [ "$paused" = "true" ]; then
    repo_active_ids="$(jq '[.[] | select(.type == "pr" and .ci == "failing") | .id]' <<<"$items")"
  else
    repo_active_ids="$(jq '[.[].id]' <<<"$items")"
  fi
  active_ids="$(jq -cn --argjson all "$active_ids" --argjson ids "$repo_active_ids" '$all + $ids')"

  new_repo_state="$(
    jq -cn \
      --arg sweep_started "$sweep_started" \
      --argjson previous "$old_repo_state" \
      --argjson items "$items" \
      --argjson policy "$policy" '
      (reduce $items[] as $item ({};
        .[$item.id] = {
          updated: $item.updated,
          fingerprint: $item.fingerprint
        }
      )) as $next_items
      | (($previous.items // {}) != $next_items) as $changed
      |
      (
        if ($previous.cursor // null) == null
        then ([$sweep_started, $items[].updated] | max)
        else ([$previous.cursor, $items[].updated] | max)
        end
      ) as $cursor
      | {
          cursor: $cursor,
          policy: $policy,
          previous_cursor: (
            if $changed
            then ($previous.cursor // "initial")
            else ($previous.previous_cursor // $previous.cursor // "initial")
            end
          ),
          items: $next_items
        }
    '
  )"
  new_state="$(
    jq -cn \
      --arg repo "$repo" \
      --argjson state "$new_state" \
      --argjson repo_state "$new_repo_state" \
      '$state | .version = 1 | .repos[$repo] = $repo_state'
  )"
done < <(jq -c '.projects[]' <<<"$config")

final_queue="$(
  jq -cn \
    --argjson existing "$existing_queue" \
    --argjson generated "$generated" \
    --argjson active_ids "$active_ids" '
    (
      $existing
      | map(. as $row
        | select($row.state == "approved" or ($active_ids | index($row.id)) != null)
      )
    ) as $still_relevant
    | reduce $generated[] as $row ($still_relevant;
        map(select(.id != $row.id)) + [$row]
      )
    | sort_by(.opened, .id)
  '
)"

queue_changes="$(
  jq -n \
    --argjson before "$existing_queue" \
    --argjson after "$final_queue" \
    'if $before == $after then 0 else
       ([($before - $after)[], ($after - $before)[]] | length)
     end'
)"

jq -c '.[]' <<<"$final_queue" >"$work/queue.jsonl"
jq -S . <<<"$new_state" >"$work/state.json"
mandate_write_if_changed "$work/queue.jsonl" "$MANDATE_QUEUE_FILE"
mandate_write_if_changed "$work/state.json" "$MANDATE_STATE_FILE"

# The state file mtime is the daily-cadence stamp. Touching it changes no
# serialized state, so a repeat sweep with no upstream activity has an empty
# content diff.
touch "$MANDATE_STATE_FILE"
echo "mandate sweep: $project_count projects; $queue_changes queue changes"
