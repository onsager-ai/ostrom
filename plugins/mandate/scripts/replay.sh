#!/usr/bin/env bash
# Read-only selector-accuracy baseline. Writes nothing.
#
# Two halves, kept structurally separate because the errors they measure are
# not symmetric — a miss is a safety failure, a false alarm is an
# interruption:
#
#   1. Replay already-merged PRs over the current selector set and report
#      the ones that touched an irreversible surface without matching any
#      bounce selector. This is a LOWER BOUND on the miss rate, not the
#      rate: a PR that touched nothing irreversible and matched nothing may
#      still have been a miss.
#   2. A per-selector table: how many currently-open items each selector is
#      responsible for classifying (from the last sweep's state), and how
#      many of the rows it produced a human dismissed via `/desk` reject
#      (from selector-events.jsonl, appended by queue.sh). Dismissals
#      attributed to no selector at all (the project default fired instead)
#      are counted separately, not folded into any selector's row.
#
# Never combined into one score. See the table, not a percentage.

set -euo pipefail
umask 077

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=mandate-lib.sh
source "$SCRIPT_DIR/mandate-lib.sh"

command -v jq >/dev/null 2>&1 || { echo "mandate replay: jq is required" >&2; exit 1; }
command -v gh >/dev/null 2>&1 || { echo "mandate replay: gh is required" >&2; exit 1; }

query_limit=200
days="${1:-30}"
case "$days" in
  ''|*[!0-9]*)
    echo "usage: replay.sh [days]" >&2
    exit 2
    ;;
esac

if ! mandate_is_configured; then
  echo "mandate replay: no mandates.yaml found at $MANDATE_USER_CONFIG or $MANDATE_REPO_CONFIG" >&2
  exit 2
fi

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

mandate_load_config >"$work/config.json" || exit
project_count="$(jq '.projects | length' "$work/config.json")"
if [ "$project_count" -eq 0 ]; then
  echo "mandate replay: mandates.yaml contains no projects" >&2
  exit 2
fi

gh_host="${GH_HOST:-github.com}"
if ! gh auth status --hostname "$gh_host" >/dev/null 2>&1; then
  echo "mandate replay: gh is not authenticated for $gh_host; run 'gh auth login'" >&2
  exit 3
fi

replay_time="${MANDATE_REPLAY_TIME:-$(date -u +%Y-%m-%dT%H:%M:%SZ)}"
cutoff="$(
  jq -rn --arg now "$replay_time" --argjson days "$days" '
    ($now | fromdateiso8601) - ($days * 86400) | todateiso8601
  '
)"

printf '%s\n' '[]' >"$work/misses.json"
capped_projects=()

while IFS= read -r project; do
  printf '%s\n' "$project" >"$work/project.json"
  repo="$(jq -r '.repo' "$work/project.json")"
  gh_error="$work/gh-error"

  if ! gh pr list --repo "$repo" --state merged --limit "$query_limit" \
    --json number,title,labels,url,files,baseRefName,mergedAt,closingIssuesReferences \
    >"$work/prs.json" 2>"$gh_error"; then
    detail="$(tr '\n' ' ' <"$gh_error")"
    echo "mandate replay: failed to query merged PRs for $repo${detail:+: $detail}" >&2
    exit 5
  fi
  if [ "$(jq 'length' "$work/prs.json")" -eq "$query_limit" ]; then
    capped_projects+=("$repo")
  fi

  if ! gh repo view "$repo" --json defaultBranchRef >"$work/repo.json" 2>"$gh_error"; then
    detail="$(tr '\n' ' ' <"$gh_error")"
    echo "mandate replay: failed to read the default branch for $repo${detail:+: $detail}" >&2
    exit 5
  fi
  default_branch="$(jq -r '.defaultBranchRef.name // ""' "$work/repo.json")"

  jq -cn \
    --arg repo "$repo" \
    --arg cutoff "$cutoff" \
    --arg default_branch "$default_branch" \
    --slurpfile prs "$work/prs.json" \
    --slurpfile project "$work/project.json" \
    --slurpfile config "$work/config.json" '
      ($project[0]) as $project
      | ($config[0]) as $config
      |
      # --- Copied verbatim from sweep.sh: same globs, same selector_match,
      # same precedence. Reused, never extended. ---
      def regex_char($char):
        if $char | IN("\\", ".", "+", "?", "^", "$", "(", ")", "[", "]", "{", "}", "|")
        then "\\" + $char
        else $char
        end;
      def glob_regex($glob; $path):
        reduce range(0; ($glob | length)) as $index
          ({body: "", skip: 0};
            if .skip > 0 then
              .skip -= 1
            else
              ($glob[$index:$index + 1]) as $char
              | if $char == "*" and $path and $glob[$index + 1:$index + 2] == "*"
                then
                  if $glob[$index + 2:$index + 3] == "/"
                  then .body += "(?:.*/)?" | .skip = 2
                  else .body += ".*" | .skip = 1
                  end
                elif $char == "*" and $path
                then .body += "[^/]*"
                elif $char == "*"
                then .body += ".*"
                else .body += regex_char($char)
                end
            end
          )
        | "^" + .body + "$";
      def glob_match($value; $glob; $path):
        $value | test(glob_regex($glob; $path); "i");
      def conventional($item):
        ((try ($item.title | capture(
          "^(?<item_type>[^(:[:space:]]+)(\\((?<item_scope>[^)]*)\\))?:"
        )) catch {}) // {}) as $parsed
        | {
            item_type: ($parsed.item_type // ""),
            scopes: (
              ($parsed.item_scope // "")
              | split(",")
              | map(gsub("^[[:space:]]+|[[:space:]]+$"; ""))
              | map(select(length > 0))
            )
          };
      def selector_match($item; $selector):
        ($item) as $bound_item
        | ($selector) as $bound_selector
        | ($bound_selector | capture("^(?<prefix>[^:]+):(?<glob>.*)$")) as $parsed
        | (conventional($bound_item)) as $conventional
        | if $parsed.prefix == "label"
          then any($bound_item.labels[]?; glob_match(.; $parsed.glob; false))
          elif $parsed.prefix == "scope"
          then any($conventional.scopes[]?; glob_match(.; $parsed.glob; false))
          elif $parsed.prefix == "type"
          then glob_match($conventional.item_type; $parsed.glob; false)
          elif $parsed.prefix == "path"
          then $bound_item.type == "pr"
            and any($bound_item.files[]?; glob_match(.; $parsed.glob; true))
          elif $parsed.prefix == "ref"
          then any($bound_item.refs[]?; ("#" + tostring) == $parsed.glob)
          elif $parsed.prefix == "title"
          then glob_match($bound_item.title; $parsed.glob; false)
          else false
          end;
      def first_selector($item; $selectors; $source):
        first(
          $selectors[]?
          | select(selector_match($item; .))
          | {source: $source, selector: .}
        ) // null;
      def bounce_match($item):
        (first_selector($item; $config.bounce_all; "bounce_all"))
        // (first_selector($item; $project.bounce; "project bounce"));
      # --- End of copied sweep.sh mechanism. ---

      def label_names:
        if type == "array" then [ .[]? | .name // empty ]
        elif type == "object" then [ (.nodes // [])[]? | .name // empty ]
        else []
        end;
      def linked_issues:
        if (.closingIssuesReferences | type) == "array"
        then .closingIssuesReferences
        elif (.closingIssuesReferences | type) == "object"
        then (.closingIssuesReferences.nodes // [])
        else []
        end;

      # Irreversible-surface heuristics. Hardcoded, not selector syntax, not
      # exposed to config authors — the mandate vocabulary gains nothing new
      # here, this is only how replay decides what counts as worth checking.
      def workflow_shaped($path):
        glob_match($path; ".github/workflows/**"; true)
        or glob_match($path; "**/release*.y*ml"; true)
        or glob_match($path; "**/.goreleaser*"; true);
      def credential_shaped($path):
        glob_match($path; "**/*credential*"; true)
        or glob_match($path; "**/*secret*"; true)
        or glob_match($path; "**/*.pem"; true)
        or glob_match($path; "**/*.key"; true)
        or glob_match($path; "**/.env"; true)
        or glob_match($path; "**/.env.*"; true)
        or glob_match($path; "**/id_rsa*"; true)
        or glob_match($path; "**/*.p12"; true)
        or glob_match($path; "**/*.pfx"; true);
      def irreversible_reasons($item):
        [
          (
            [$item.files[]? | select(workflow_shaped(.))]
            | if length > 0 then "workflow file: " + .[0] else empty end
          ),
          (
            [$item.files[]? | select(credential_shaped(.))]
            | if length > 0 then "credential-shaped path: " + .[0] else empty end
          )
          # Deliberately NOT a trigger: a base branch equal to the default one.
          # #17 meant a change reaching the default branch WITHOUT review, i.e.
          # a direct push. Read as "PR merged into main" it fires on every
          # ordinary PR in a trunk-based repo: measured against the real
          # portfolio it flagged 235 of 277 items on that condition alone,
          # burying the 41 citing an actual content signal. A trigger that
          # fires on nearly everything cannot discriminate, and the miss count
          # it produces is unreadable. Detecting genuine review bypass needs
          # commit-level data, a default-branch commit with no associated PR,
          # which is a separate signal and not derivable from this PR scan.
        ];

      [
        $prs[0][]
        | . as $raw
        | {
            id: ($repo + "#" + (.number | tostring)),
            repo: $repo,
            number: .number,
            ref: ("#" + (.number | tostring)),
            type: "pr",
            title: (
              (.title // "")
              | if length > 0 then . else "(title unavailable)" end
            ),
            labels: (
              ((.labels // []) | label_names)
              + [linked_issues[]? | ((.labels // []) | label_names)[]]
              | unique
            ),
            refs: ([.number] + [linked_issues[]? | .number] | unique),
            files: [(.files // [])[]? | .path // empty] | unique,
            base: (.baseRefName // ""),
            merged: (.mergedAt // ""),
            url: (.url // "")
          }
        | select(.merged != "" and .merged >= $cutoff)
        | . + {irreversible: (irreversible_reasons(.) | map(select(. != null)))}
        | select((.irreversible | length) > 0)
        | select(bounce_match(.) == null)
        | {id, repo, ref, title, merged, url, irreversible}
      ]
    ' >"$work/repo-misses.json"

  jq -cn --slurpfile all "$work/misses.json" --slurpfile repo "$work/repo-misses.json" \
    '$all[0] + $repo[0]' >"$work/next.json"
  mv "$work/next.json" "$work/misses.json"
done < <(jq -c '.projects[]' "$work/config.json")

echo "mandate replay: lower bound only"
echo "A PR that touched nothing irreversible and matched no selector may still have been a miss; this only counts what it can see."
echo

echo "MISSES (lower bound) — merged PRs in the last $days days that touched an irreversible surface and matched no bounce selector:"
miss_count="$(jq 'length' "$work/misses.json")"
if [ "$miss_count" -eq 0 ]; then
  echo "  none flagged"
else
  jq -r '
    sort_by(.merged)[]
    | .id + "  " + .title + " — merged " + .merged
      + "; " + (.irreversible | join("; ")) + " — " + .url
  ' "$work/misses.json"
fi
echo

if [ "${#capped_projects[@]}" -gt 0 ]; then
  for repo in "${capped_projects[@]}"; do
    echo "$repo: merged-PR query hit the $query_limit-item cap — the $days-day window may be incomplete"
  done
  echo
fi

# --- Per-selector report ---
state_json='{}'
if [ -s "$MANDATE_STATE_FILE" ]; then
  state_json="$(cat "$MANDATE_STATE_FILE")"
fi
events_json='[]'
if [ -s "$MANDATE_EVENTS_FILE" ]; then
  events_json="$(jq -s '.' "$MANDATE_EVENTS_FILE")"
fi

jq -cn \
  --slurpfile config "$work/config.json" \
  --argjson state "$state_json" \
  --argjson events "$events_json" '
    ($config[0]) as $config
    |
    def tier($selector):
      ($selector | capture("^(?<prefix>[^:]+):").prefix) as $prefix
      | if $prefix == "path" or $prefix == "ref"
        then "content-derived"
        else "author-written"
        end;
    def selector_records:
      ($config.bounce_all[]? | {repo: null, source: "bounce_all", selector: .}),
      ($config.projects[]? as $project
        | ($project.bounce[]? | {repo: $project.repo, source: "project bounce", selector: .}),
          ($project.excluded[]? | {repo: $project.repo, source: "excluded", selector: .}),
          ($project.delegated[]? | {repo: $project.repo, source: "delegated", selector: .})
      );
    ([
      ($state.repos // {}) | to_entries[]
      | .key as $repo
      | (.value.items // {}) | to_entries[]
      | {repo: $repo, matched_selector: .value.matched_selector}
    ]) as $current_items
    | ([
        $events[]?
        | select(.decision == "reject")
        | {repo: (.id | split("#")[0]), matched_selector: .matched_selector}
      ]) as $rejections
    | [
        selector_records
        | . as $record
        | (
            [$current_items[]
              | select(.matched_selector == $record.selector)
              | select($record.repo == null or .repo == $record.repo)
            ] | length
          ) as $fired
        | (
            [$rejections[]
              | select(.matched_selector == $record.selector)
              | select($record.repo == null or .repo == $record.repo)
            ] | length
          ) as $dismissed
        | {
            tier: tier($record.selector),
            repo: ($record.repo // "*"),
            source: $record.source,
            selector: $record.selector,
            fired: $fired,
            dismissed: $dismissed
          }
      ]
      | sort_by([.repo, .source, .selector])
  ' >"$work/report.json"

echo "PER-SELECTOR REPORT"
echo "tier	repo	source	selector	fired	dismissed"
jq -r '.[] | [.tier, .repo, .source, .selector, (.fired|tostring), (.dismissed|tostring)] | @tsv' \
  "$work/report.json"
echo
echo "fired = open items the last sweep classified via that selector (current snapshot, not lifetime history)."
echo "dismissed = /desk rejections of a row that selector produced, recorded in selector-events.jsonl."
echo "path: only applies to PRs (sweep.sh gates it on \$item.type == \"pr\"); for issues every prefix above is author-written and there is no content-derived gating at all."

no_selector_dismissals="$(
  jq '
    [.[] | select(
      .decision == "reject"
      and ((.matched_selector // "") | startswith("default:"))
    )] | length
  ' <<<"$events_json"
)"
echo
echo "Dismissals attributed to no selector (the project default fired instead): $no_selector_dismissals"
echo "These are not a selector's false alarms and are kept out of the table above."
echo
echo "Unmatched irreversible-surface merges (misses, lower bound): $miss_count"
echo "This is a separate count from the dismissal figures above — it measures misses, not false alarms, and is not combined with them into any single score."
