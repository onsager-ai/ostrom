#!/usr/bin/env bash
# Read-only merged-SHA gate audit. Writes nothing.
#
# For every roster repository, joins merged PRs in the requested window to
# gate.jsonl by both PR identity and the head SHA that actually merged. The
# last recorded verdict at that exact SHA is the verdict counted here. A pass
# with an excused condition is shown as a subset of passes, not as a separate
# outcome.
#
# The unknowns stay separate because they mean different things:
#
#   1. No verdict at any SHA means the gate was never run for that PR.
#   2. Verdicts at non-merged SHAs mean the gate ran, but not on what merged.
#   3. Verdicts carrying only a null head_sha cannot be SHA-joined at all.
#
# Null-SHA records are also counted as records, including when another record
# for the same PR can be joined. Counts cover only PRs merged in the window;
# gate records for unmerged or out-of-window PRs are excluded.
#
# Never combined into one score. See the tables, not a percentage.

set -euo pipefail
umask 077

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=mandate-lib.sh
source "$SCRIPT_DIR/mandate-lib.sh"

command -v jq >/dev/null 2>&1 || { echo "mandate audit: jq is required" >&2; exit 1; }
command -v gh >/dev/null 2>&1 || { echo "mandate audit: gh is required" >&2; exit 1; }

query_limit=200
days="${1:-30}"
case "$days" in
  ''|*[!0-9]*)
    echo "usage: audit.sh [days]" >&2
    exit 2
    ;;
esac

if ! mandate_is_configured; then
  echo "mandate audit: no mandates.yaml found at $MANDATE_USER_CONFIG or $MANDATE_REPO_CONFIG" >&2
  exit 2
fi

config="$(mandate_load_config)" || exit
project_count="$(printf '%s\n' "$config" | jq '.projects | length')"
if [ "$project_count" -eq 0 ]; then
  echo "mandate audit: mandates.yaml contains no projects" >&2
  exit 2
fi

gh_host="${GH_HOST:-github.com}"
if ! gh auth status --hostname "$gh_host" >/dev/null 2>&1; then
  echo "mandate audit: gh is not authenticated for $gh_host; run 'gh auth login'" >&2
  exit 3
fi

audit_time="${MANDATE_AUDIT_TIME:-$(date -u +%Y-%m-%dT%H:%M:%SZ)}"
cutoff="$(
  jq -rn --arg now "$audit_time" --argjson days "$days" '
    ($now | fromdateiso8601) - ($days * 86400) | todateiso8601
  '
)"

gate_source=/dev/null
gate_log_state="present"
if [ -e "$MANDATE_GATE_LOG" ]; then
  if [ ! -r "$MANDATE_GATE_LOG" ]; then
    echo "mandate audit: gate log exists but is not readable: $MANDATE_GATE_LOG" >&2
    exit 4
  fi
  if [ -s "$MANDATE_GATE_LOG" ]; then
    gate_source="$MANDATE_GATE_LOG"
  else
    gate_log_state="empty"
  fi
else
  gate_log_state="absent"
fi

echo "mandate audit: merged-SHA gate verdicts over the last $days days"
echo "Window: $cutoff through $audit_time (by mergedAt)."
if [ "$gate_log_state" != "present" ]; then
  echo "Gate log is $gate_log_state at $MANDATE_GATE_LOG: every count below reflects no recorded verdicts, not a measurement of zero."
fi
echo "Pass-with-excuse is a subset of pass; unknown join states remain separate."
echo

while IFS= read -r project; do
  repo="$(printf '%s\n' "$project" | jq -r '.repo')"

  if ! merged_prs="$(
    gh pr list --repo "$repo" --state merged --limit "$query_limit" \
      --json number,mergedAt,headRefOid,mergeCommit
  )"; then
    echo "mandate audit: failed to query merged PRs for $repo" >&2
    exit 5
  fi

  query_count="$(printf '%s\n' "$merged_prs" | jq 'length')"
  report="$(
    printf '%s\n' "$merged_prs" |
      jq -c \
        --arg repo "$repo" \
        --arg cutoff "$cutoff" \
        --slurpfile gate "$gate_source" '
          def merge_commit_oid:
            if (.mergeCommit | type) == "object"
            then .mergeCommit.oid // ""
            elif (.mergeCommit | type) == "string"
            then .mergeCommit
            else ""
            end;
          def joined_merges:
            (reduce $gate[] as $record ({}; .[$record.pr] += [$record])) as $gate_index
            | [
              .[]
              | select((.mergedAt // "") != "" and .mergedAt >= $cutoff)
              | . as $pr
              | ($repo + "#" + ($pr.number | tostring)) as $id
              | ($pr.headRefOid // "") as $merged_sha
              | ($gate_index[$id] // []) as $records
              | [
                  $records[]
                  | select((.head_sha | type) == "string" and (.head_sha | length) > 0)
                ] as $sha_records
              | [$sha_records[] | select(.head_sha == $merged_sha)] as $at_merged_sha
              | ($at_merged_sha | last // null) as $latest
              | {
                  pr: $id,
                  merged_at: $pr.mergedAt,
                  merged_sha: $merged_sha,
                  merge_commit: ($pr | merge_commit_oid),
                  records: $records,
                  latest: $latest,
                  outcome: (
                    if $merged_sha == "" then "missing_merged_sha"
                    elif ($records | length) == 0 then "no_verdict"
                    elif $latest != null then
                      if ($latest.verdict | IN("pass", "fail", "inconclusive"))
                      then $latest.verdict
                      else "unknown_verdict"
                      end
                    elif ($sha_records | length) > 0 then "other_sha"
                    else "null_sha_only"
                    end
                  )
                }
            ];
          joined_merges as $merges
          | {
              total: ($merges | length),
              no_verdict: ([$merges[] | select(.outcome == "no_verdict")] | length),
              null_sha_only: ([$merges[] | select(.outcome == "null_sha_only")] | length),
              other_sha: ([$merges[] | select(.outcome == "other_sha")] | length),
              missing_merged_sha: ([$merges[] | select(.outcome == "missing_merged_sha")] | length),
              pass: ([$merges[] | select(.outcome == "pass")] | length),
              pass_excused: ([
                $merges[]
                | select(.outcome == "pass")
                | select(any(.latest.conditions[]?; .result == "excused"))
              ] | length),
              fail: ([$merges[] | select(.outcome == "fail")] | length),
              inconclusive: ([$merges[] | select(.outcome == "inconclusive")] | length),
              unknown_verdict: ([$merges[] | select(.outcome == "unknown_verdict")] | length),
              null_sha_records: ([
                $merges[].records[]?
                | select(.head_sha == null)
              ] | length),
              prs_with_null_sha_records: ([
                $merges[]
                | select(any(.records[]?; .head_sha == null))
              ] | length),
              failed_conditions: ([
                $merges[]
                | select(.outcome == "fail")
                | [.latest.conditions[]? | select(.result == "fail") | .name] | unique[]
              ] | sort | group_by(.) | map({name: .[0], count: length})),
              inconclusive_conditions: ([
                $merges[]
                | select(.outcome == "inconclusive")
                | [.latest.conditions[]? | select(.result == "inconclusive") | .name] | unique[]
              ] | sort | group_by(.) | map({name: .[0], count: length})),
              unknowns: [
                $merges[]
                | select(.outcome | IN(
                    "no_verdict", "null_sha_only", "other_sha",
                    "missing_merged_sha", "unknown_verdict"
                  ))
                | {
                    pr,
                    outcome,
                    merged_sha,
                    merge_commit,
                    null_sha_records: ([.records[]? | select(.head_sha == null)] | length)
                  }
              ],
              non_passes: [
                $merges[]
                | select(.outcome == "fail" or .outcome == "inconclusive")
                | {pr, verdict: .outcome, merged_sha, merge_commit}
              ]
            }
        '
  )"

  echo "REPOSITORY $repo"
  echo "MERGE OUTCOMES"
  echo "bucket	count"
  printf '%s\n' "$report" | jq -r '["no verdict at any SHA", (.no_verdict | tostring)] | @tsv'
  printf '%s\n' "$report" | jq -r '["only null-SHA verdicts (unjoinable)", (.null_sha_only | tostring)] | @tsv'
  printf '%s\n' "$report" | jq -r '["verdict exists, but none at the merged SHA", (.other_sha | tostring)] | @tsv'
  printf '%s\n' "$report" | jq -r '["merged PR missing headRefOid", (.missing_merged_sha | tostring)] | @tsv'
  printf '%s\n' "$report" | jq -r '["pass at the merged SHA", (.pass | tostring)] | @tsv'
  printf '%s\n' "$report" | jq -r '["of passes, contains an excused condition", (.pass_excused | tostring)] | @tsv'
  printf '%s\n' "$report" | jq -r '["fail or inconclusive at the merged SHA", ((.fail + .inconclusive) | tostring)] | @tsv'
  printf '%s\n' "$report" | jq -r '["  fail", (.fail | tostring)] | @tsv'
  printf '%s\n' "$report" | jq -r '["  inconclusive", (.inconclusive | tostring)] | @tsv'
  printf '%s\n' "$report" | jq -r '["unrecognized verdict at the merged SHA", (.unknown_verdict | tostring)] | @tsv'
  printf '%s\n' "$report" | jq -r '["total merged PRs in window", (.total | tostring)] | @tsv'
  echo

  echo "RECORD QUALITY"
  echo "measure	count"
  printf '%s\n' "$report" | jq -r '["null head_sha records for merged PRs in window", (.null_sha_records | tostring)] | @tsv'
  printf '%s\n' "$report" | jq -r '["merged PRs touched by a null head_sha record", (.prs_with_null_sha_records | tostring)] | @tsv'
  echo

  echo "FAILED CONDITION BREAKDOWN"
  echo "condition	merged PRs"
  if [ "$(printf '%s\n' "$report" | jq '.failed_conditions | length')" -eq 0 ]; then
    echo "none	0"
  else
    printf '%s\n' "$report" |
      jq -r '.failed_conditions[] | [.name, (.count | tostring)] | @tsv'
  fi
  echo

  echo "INCONCLUSIVE CONDITION BREAKDOWN"
  echo "condition	merged PRs"
  if [ "$(printf '%s\n' "$report" | jq '.inconclusive_conditions | length')" -eq 0 ]; then
    echo "none	0"
  else
    printf '%s\n' "$report" |
      jq -r '.inconclusive_conditions[] | [.name, (.count | tostring)] | @tsv'
  fi
  echo

  echo "UNKNOWN JOIN DETAILS"
  echo "pr	reason	merged head	merge commit	null-SHA records"
  if [ "$(printf '%s\n' "$report" | jq '.unknowns | length')" -eq 0 ]; then
    echo "none	-	-	-	0"
  else
    printf '%s\n' "$report" |
      jq -r '
        def reason:
          if .outcome == "no_verdict" then "no verdict at any SHA"
          elif .outcome == "null_sha_only" then "only null-SHA verdicts"
          elif .outcome == "other_sha" then "none at merged SHA"
          elif .outcome == "missing_merged_sha" then "missing headRefOid"
          else "unrecognized verdict"
          end;
        .unknowns[]
        | [.pr, reason, (.merged_sha // ""), (.merge_commit // ""), (.null_sha_records | tostring)]
        | map(if . == "" then "-" else . end)
        | @tsv
      '
  fi
  echo

  echo "NON-PASS VERDICTS AT THE MERGED SHA"
  echo "pr	verdict	merged head	merge commit"
  if [ "$(printf '%s\n' "$report" | jq '.non_passes | length')" -eq 0 ]; then
    echo "none	-	-	-"
  else
    printf '%s\n' "$report" |
      jq -r '.non_passes[] | [.pr, .verdict, .merged_sha, .merge_commit] | @tsv'
  fi
  echo

  if [ "$query_count" -eq "$query_limit" ]; then
    echo "$repo: merged-PR query hit the $query_limit-item cap — the $days-day window may be incomplete"
    echo
  fi
done < <(printf '%s\n' "$config" | jq -c '.projects[]')

echo "No score is calculated: outcome, exception use, join coverage, and condition failures are separate counts."
