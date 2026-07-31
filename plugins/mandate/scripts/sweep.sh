#!/usr/bin/env bash
# Read-only GitHub portfolio sweep. The only writes are private queue/state.

set -euo pipefail
umask 077

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=mandate-lib.sh
source "$SCRIPT_DIR/mandate-lib.sh"

command -v jq >/dev/null 2>&1 || { echo "mandate sweep: jq is required" >&2; exit 1; }
command -v gh >/dev/null 2>&1 || { echo "mandate sweep: gh is required" >&2; exit 1; }

query_limit=200

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
  old_state='{"version":2,"repos":{},"dead_selectors":[]}'
fi

sweep_started="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
generated='[]'
active_ids='[]'
current_items='[]'
selector_stats='[]'
new_state="$old_state"

while IFS= read -r project; do
  repo="$(jq -r '.repo' <<<"$project")"
  gh_error="$work/gh-error"

  if ! issues="$(gh issue list --repo "$repo" --state open --limit "$query_limit" \
    --json number,title,labels,createdAt,updatedAt,url 2>"$gh_error")"; then
    detail="$(tr '\n' ' ' <"$gh_error")"
    echo "mandate sweep: failed to query open issues for $repo${detail:+: $detail}" >&2
    exit 5
  fi
  if ! prs="$(gh pr list --repo "$repo" --state open --limit "$query_limit" \
    --json number,title,labels,createdAt,updatedAt,url,isDraft,reviewDecision,statusCheckRollup,closingIssuesReferences,files \
    2>"$gh_error")"; then
    detail="$(tr '\n' ' ' <"$gh_error")"
    echo "mandate sweep: failed to query open PRs and CI for $repo${detail:+: $detail}" >&2
    exit 5
  fi
  item_cap='null'
  if [ "$(jq 'length' <<<"$issues")" -eq "$query_limit" ] ||
    [ "$(jq 'length' <<<"$prs")" -eq "$query_limit" ]; then
    item_cap="$query_limit"
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
      def normalized($type):
        . as $item
        | (if $type == "pr" then ($item | ci_state) else "none" end) as $ci
        | (if $type == "pr" then ($item | linked_issues) else [] end) as $linked
        | {
            id: ($repo + "#" + (.number | tostring)),
            repo: $repo,
            number: .number,
            ref: ("#" + (.number | tostring)),
            type: $type,
            title: (
              ($item.title // "")
              | if length > 0 then . else "(title unavailable)" end
            ),
            labels: (
              ((.labels // []) | label_names)
              + [$linked[]? | ((.labels // []) | label_names)[]]
              | unique
            ),
            refs: ([.number] + [$linked[]? | .number] | unique),
            closing_refs: (
              if $type == "pr"
              then [$linked[]? | .number] | unique
              else []
              end
            ),
            files: (
              if $type == "pr"
              then [(.files // [])[]? | .path // empty] | unique
              else []
              end
            ),
            opened: .createdAt,
            updated: .updatedAt,
            ci: $ci,
            ready: ($type == "pr" and (.isDraft | not) and $ci == "passing"),
            review: (.reviewDecision // "")
          }
        | .fingerprint = ([
            .title,
            (.labels | sort | join(",")),
            (.refs | sort | map(tostring) | join(",")),
            (.files | sort | join(",")),
            .ci,
            (.ready | tostring),
            .review
          ] | join("|"));
      [($issues[] | normalized("issue")), ($prs[] | normalized("pr"))]
    '
  )"

  policy="$(
    jq -cn \
      --argjson project "$project" \
      --argjson bounce_all "$(jq -c '.bounce_all' <<<"$config")" '
      {
        delegated: $project.delegated,
        excluded: $project.excluded,
        reserved: $project.reserved,
        bounce: $project.bounce,
        bounce_all: $bounce_all,
        default: $project.default
      } as $selectors
      | ($selectors | tojson | explode
          | reduce .[] as $code (0; ((. * 31 + $code) % 2147483647))
          | tostring) as $selector_hash
      | $selectors + {
          paused: $project.paused,
          selector_hash: $selector_hash
        }
    '
  )"

  old_repo_state="$(jq -c --arg repo "$repo" '.repos[$repo] // {}' <<<"$old_state")"
  analysis="$(
    jq -cn \
      --arg sweep_started "$sweep_started" \
      --argjson project "$project" \
      --argjson config "$config" \
      --argjson items "$items" \
      --argjson policy "$policy" \
      --argjson item_cap "$item_cap" \
      --argjson previous "$old_repo_state" '
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
      def classify($item):
        (first(
          $project.reserved[]? as $number
          | select(any($item.refs[]?; . == $number))
          | {terminal: "reserved", source: "reserved", selector: ("ref:#" + ($number | tostring))}
        ) // null) as $reserved
        | (first_selector($item; $config.bounce_all; "bounce_all")) as $shared_bounce
        | (first_selector($item; $project.bounce; "project bounce")) as $project_bounce
        | (first_selector($item; $project.excluded; "excluded")) as $excluded
        | (first_selector($item; $project.delegated; "delegated")) as $delegated
        | if $reserved != null then $reserved
          elif $shared_bounce != null then $shared_bounce + {terminal: "tripwire"}
          elif $project_bounce != null then $project_bounce + {terminal: "tripwire"}
          elif $excluded != null then $excluded + {terminal: "excluded"}
          elif $delegated != null then $delegated + {terminal: "delegated"}
          else {
            terminal: $project.default,
            source: "default",
            selector: ("default:" + $project.default)
          }
          end;
      def selector_stats:
        ($config.bounce_all[]? | . as $selector | {
          repo: null,
          source: "bounce_all",
          selector: $selector,
          hit: any($items[]?; selector_match(.; $selector))
        }),
        ($project.bounce[]? | . as $selector | {
          repo: $project.repo,
          source: "project bounce",
          selector: $selector,
          hit: any($items[]?; selector_match(.; $selector))
        }),
        ($project.excluded[]? | . as $selector | {
          repo: $project.repo,
          source: "excluded",
          selector: $selector,
          hit: any($items[]?; selector_match(.; $selector))
        }),
        ($project.delegated[]? | . as $selector | {
          repo: $project.repo,
          source: "delegated",
          selector: $selector,
          hit: any($items[]?; selector_match(.; $selector))
        });
      def mandate_record($item; $kind; $reason):
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
      def match_reason($classified):
        if $classified.source == "default"
        then $classified.selector
        else $classified.source + " " + $classified.selector
        end;
      def shadowed_issue($item; $active):
        $item.type == "issue"
        and any($active[]?;
          .type == "pr"
          and any(.closing_refs[]?; . == $item.number)
        );
      def closing_suffix($item; $active):
        ([
          $item.closing_refs[]? as $closed
          | select(any($active[]?;
              .type == "issue" and .number == $closed
            ))
          | $closed
        ] | unique) as $closed
        | if ($closed | length) == 0 then ""
          else " (closes " + ($closed | map("#" + tostring) | join(", ")) + ")"
          end;

      (($previous.cursor // null) == null) as $initial
      | (($previous.selector_hash // "") != $policy.selector_hash) as $policy_changed
      | [
          $items[]
          | . as $item
          | classify($item) as $classification
          | ($previous.items[$item.id] // null) as $old
          | (
              if $initial or $policy_changed
              then $sweep_started
              else ($old.first_seen // $sweep_started)
              end
            ) as $first_seen
          | (([
              ($first_seen | fromdateiso8601),
              ($item.updated | fromdateiso8601)
            ] | max)) as $movement_clock
          | . + {
              classification: $classification,
              first_seen: $first_seen,
              stuck: (
                ($initial | not)
                and ($policy_changed | not)
                and $classification.terminal == "delegated"
                and (($sweep_started | fromdateiso8601) - $movement_clock)
                  >= ($config.stuck_after_days * 86400)
              ),
              old: $old
            }
        ] as $classified
      | ([
          $classified[]
          | select(
              if $initial or $policy_changed then
                .classification.terminal == "reserved"
                or .classification.terminal == "tripwire"
                or (.type == "pr" and .ci == "failing")
              else
                .classification.terminal == "reserved"
                or .classification.terminal == "tripwire"
                or (.type == "pr" and .ci == "failing")
                or (($project.paused | not) and .classification.terminal == "delegated")
              end
            )
        ]) as $active
      | ([
          $active[]
          | . as $item
          | select(shadowed_issue($item; $active) | not)
        ]) as $visible_active
      | ([
          $classified[]
          | . as $item
          | (
              $item.old == null
              or $item.old.fingerprint != $item.fingerprint
              or $item.updated > ($previous.cursor // "")
              or ($item.stuck and (($item.old.stuck // false) | not))
            ) as $event
          | (
              $item.classification.terminal == "reserved"
              or $item.classification.terminal == "tripwire"
              or ($item.type == "pr" and $item.ci == "failing")
            ) as $safety
          | select(
              if $initial or $policy_changed then $safety
              else $event and (
                $safety
                or (($project.paused | not) and $item.classification.terminal == "delegated")
              )
              end
            )
          | select(shadowed_issue($item; $active) | not)
          | (
              if $item.classification.terminal == "reserved" then
                {
                  kind: "decision",
                  reason: ("reserved " + $item.classification.selector)
                }
              elif $item.classification.terminal == "tripwire" then
                {
                  kind: "tripwire",
                  reason: ("tripwire: " + match_reason($item.classification))
                }
              elif $item.type == "pr" and $item.ci == "failing" then
                {
                  kind: "drift",
                  reason: (
                    "CI is failing; "
                    + match_reason($item.classification)
                  )
                }
              elif $item.stuck then
                {
                  kind: "stuck",
                  reason: (
                    match_reason($item.classification)
                    + "; no movement for "
                    + ($config.stuck_after_days | tostring)
                    + " days"
                  )
                }
              elif $item.ready then
                {
                  kind: "decision",
                  reason: (
                    match_reason($item.classification)
                    + "; open PR passed CI"
                  )
                }
              else
                {
                  kind: "moved",
                  reason: match_reason($item.classification)
                }
              end
            ) as $row
          | (closing_suffix($item; $active)) as $closing_suffix
          | {
              id: $item.id,
              repo: $item.repo,
              ref: $item.ref,
              title: $item.title,
              kind: $row.kind,
              mandate: mandate_record(
                $item;
                $row.kind;
                ($row.reason + $closing_suffix)
              ),
              state: "pending",
              opened: $item.opened
            }
        ]) as $rows
      | ([$visible_active[] | .id]) as $active_ids
      | (reduce $classified[] as $item ({};
          .[$item.id] = {
            updated: $item.updated,
            fingerprint: $item.fingerprint,
            first_seen: $item.first_seen,
            classification: $item.classification.terminal,
            matched_selector: $item.classification.selector,
            stuck: $item.stuck
          }
        )) as $next_items
      | (($previous.items // {}) != $next_items) as $changed
      | ([
          $classified[]
          | select(
              .classification.terminal == "delegated"
              and (.old.classification // "") != "delegated"
            )
          | .id
        ]) as $entered
      | ([
          ($previous.items // {}) | to_entries[]
          | select(
              .value.classification == "delegated"
              and (($next_items[.key].classification // "") != "delegated")
            )
          | .key
        ]) as $left
      | (
          if $initial then
            {
              kind: "baseline",
              reported: false,
              text: ($project.repo + ": baselined " + ($items | length | tostring) + " open items")
            }
          elif $policy_changed then
            {
              kind: "policy",
              reported: false,
              text: (
                $project.repo
                + ": mandate changed — "
                + ($entered | length | tostring)
                + " items entered scope, "
                + ($left | length | tostring)
                + " left"
              )
            }
          elif $changed then null
          else ($previous.notice // null)
          end
        ) as $notice
      | (
          if $initial then ([$sweep_started, $items[].updated] | max)
          else ([$previous.cursor, $items[].updated] | max)
          end
        ) as $cursor
      | {
          rows: $rows,
          active_ids: $active_ids,
          current_items: [
            $classified[]
            | . as $item
            | {
                id: .id,
                title: .title,
                closing_suffix: closing_suffix($item; $active)
              }
          ],
          selector_stats: [selector_stats],
          repo_state: {
            cursor: $cursor,
            previous_cursor: (
              if $changed
              then ($previous.cursor // "initial")
              else ($previous.previous_cursor // $previous.cursor // "initial")
              end
            ),
            selector_hash: $policy.selector_hash,
            policy: $policy,
            notice: $notice,
            unclassified: (
              [$classified[] | select(.classification.terminal == "unclassified")] | length
            ),
            item_cap: $item_cap,
            scope_changes: (
              if $policy_changed and ($initial | not)
              then {entered: $entered, left: $left}
              elif $changed
              then {entered: [], left: []}
              else ($previous.scope_changes // {entered: [], left: []})
              end
            ),
            items: $next_items
          }
        }
    '
  )"

  rows="$(jq -c '.rows' <<<"$analysis")"
  generated="$(jq -cn --argjson all "$generated" --argjson rows "$rows" '$all + $rows')"
  repo_active_ids="$(jq -c '.active_ids' <<<"$analysis")"
  active_ids="$(jq -cn --argjson all "$active_ids" --argjson ids "$repo_active_ids" '$all + $ids')"
  repo_current_items="$(jq -c '.current_items' <<<"$analysis")"
  current_items="$(
    jq -cn \
      --argjson all "$current_items" \
      --argjson items "$repo_current_items" \
      '$all + $items'
  )"
  repo_selector_stats="$(jq -c '.selector_stats' <<<"$analysis")"
  selector_stats="$(
    jq -cn --argjson all "$selector_stats" --argjson stats "$repo_selector_stats" \
      '$all + $stats'
  )"
  new_repo_state="$(jq -c '.repo_state' <<<"$analysis")"
  new_state="$(
    jq -cn \
      --arg repo "$repo" \
      --argjson state "$new_state" \
      --argjson repo_state "$new_repo_state" \
      '$state | .version = 2 | .repos[$repo] = $repo_state'
  )"
done < <(jq -c '.projects[]' <<<"$config")

dead_selectors="$(
  jq -cn --argjson stats "$selector_stats" '
    $stats
    | sort_by([(.repo // ""), .source, .selector])
    | group_by([.repo, .source, .selector])
    | map(
        select(any(.[]; .hit) | not)
        | first
        | del(.hit)
      )
  '
)"
configured_repos="$(jq -c '[.projects[].repo]' <<<"$config")"
new_state="$(
  jq -cn \
    --argjson state "$new_state" \
    --argjson dead "$dead_selectors" \
    --argjson configured_repos "$configured_repos" '
    $state
    | .dead_selectors = $dead
    | .repos |= with_entries(
        select(.key as $repo | ($configured_repos | index($repo)) != null)
      )
  '
)"

final_queue="$(
  jq -cn \
    --argjson existing "$existing_queue" \
    --argjson generated "$generated" \
    --argjson active_ids "$active_ids" \
    --argjson current_items "$current_items" '
    def current($id):
      first($current_items[] | select(.id == $id)) // null;
    def enrich:
      . as $row
      | (current($row.id)) as $current
      | if .kind == "moved"
        then
          if (.mandate | type) == "object"
          then .mandate.reason |= sub("; updated since the read cursor$"; "")
          else .mandate |= sub("; updated since the read cursor$"; "")
          end
        else .
        end
      | .title = ($current.title // .title // "(title unavailable)")
      | if ($current.closing_suffix // "") != ""
          and (
            (.mandate.reason // .mandate // "")
            | endswith($current.closing_suffix)
            | not
          )
        then
          if (.mandate | type) == "object"
          then .mandate.reason += $current.closing_suffix
          else .mandate += $current.closing_suffix
          end
        else .
        end;
    (
      $existing
      | map(. as $row
        | select($row.state == "approved" or ($active_ids | index($row.id)) != null)
      )
    ) as $still_relevant
    | reduce $generated[] as $row ($still_relevant;
        map(select(.id != $row.id)) + [$row]
      )
    | map(enrich)
    | sort_by(.opened, .id)
  '
)"

queue_changes="$(
  jq -n \
    --argjson before "$existing_queue" \
    --argjson after "$final_queue" '
    (
      $before
      | map(. as $row
        | if has("title") then .
          else
            (
              first($after[] | select(.id == $row.id)).title
              // "(title unavailable)"
            ) as $title
            | . + {title: $title}
          end
        )
    ) as $comparable_before
    | if $comparable_before == $after then 0
      else ([
        ($comparable_before - $after)[],
        ($after - $comparable_before)[]
      ] | length)
      end
  '
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
