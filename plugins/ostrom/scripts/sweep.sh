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
full_reconciliation_seconds=$((24 * 60 * 60))

if ! mandate_is_configured; then
  echo "mandate sweep: no mandates.yaml found at $MANDATE_USER_CONFIG or $MANDATE_REPO_CONFIG" >&2
  exit 2
fi

# Fetch GitHub's issues change feed one page at a time so the first page's
# validator can be persisted and reused. `gh api --include` deliberately keeps
# the HTTP status and ETag beside the JSON body: a conditional 304 is a normal,
# empty delta, while every other non-2xx response is a sweep fault.
fetch_issue_page() {
  local repo="$1"
  local since="$2"
  local page="$3"
  local etag="$4"
  local response="$work/issues-response-$page"
  local body="$work/issues-page-$page.json"
  local endpoint status detail
  local -a headers=()

  endpoint="repos/$repo/issues?state=open&sort=updated&direction=asc&per_page=100&page=$page"
  if [ -n "$since" ]; then
    endpoint="$endpoint&since=$since"
  fi
  if [ "$page" -eq 1 ] && [ -n "$etag" ]; then
    headers=(-H "If-None-Match: $etag")
  fi

  if ! gh api -X GET --include "${headers[@]}" "$endpoint" \
    >"$response" 2>"$work/gh-error"; then
    status="$(awk 'toupper($1) ~ /^HTTP\// {code=$2} END {print code}' "$response")"
    if [ "$status" != "304" ]; then
      detail="$(tr '\n' ' ' <"$work/gh-error")"
      echo "mandate sweep: failed to query the issues change feed for $repo${detail:+: $detail}" >&2
      return 5
    fi
  fi

  status="$(awk 'toupper($1) ~ /^HTTP\// {code=$2} END {print code}' "$response")"
  case "$status" in
    304)
      printf '%s\n' '[]' >"$body"
      ;;
    2??)
      awk '
        toupper($1) ~ /^HTTP\// { body = 0; next }
        body { print }
        /^[[:space:]]*$/ { body = 1 }
      ' "$response" >"$body"
      if ! jq -e 'type == "array"' "$body" >/dev/null; then
        echo "mandate sweep: issues change feed for $repo returned a non-array body" >&2
        return 5
      fi
      ;;
    *)
      detail="$(tr '\n' ' ' <"$work/gh-error")"
      echo "mandate sweep: issues change feed for $repo returned HTTP ${status:-unknown}${detail:+: $detail}" >&2
      return 5
      ;;
  esac

  if [ "$page" -eq 1 ]; then
    issue_http_status="$status"
    issue_etag="$(awk '
      tolower($1) == "etag:" {
        sub(/^[^:]*:[[:space:]]*/, "")
        sub(/\r$/, "")
        value = $0
      }
      END { print value }
    ' "$response")"
  fi
}

fetch_issues() {
  local repo="$1"
  local since="$2"
  local previous_etag="$3"
  local first_count second_count

  issue_http_status=""
  issue_etag=""
  fetch_issue_page "$repo" "$since" 1 "$previous_etag" || return $?
  first_count="$(jq 'length' "$work/issues-page-1.json")"
  if [ "$issue_http_status" = "304" ] || [ "$first_count" -lt 100 ]; then
    cp "$work/issues-page-1.json" "$work/issues.json"
    return
  fi

  fetch_issue_page "$repo" "$since" 2 "" || return $?
  second_count="$(jq 'length' "$work/issues-page-2.json")"
  jq -cn \
    --slurpfile first "$work/issues-page-1.json" \
    --slurpfile second "$work/issues-page-2.json" \
    '$first[0] + $second[0]' >"$work/issues.json"
  # A page-one ETag validates only page one, not the entire delta. Do not
  # reuse it when pagination was necessary or a later page could change while
  # page one stayed byte-identical.
  issue_etag=""
  if [ "$first_count" -eq 100 ] && [ "$second_count" -eq 100 ]; then
    echo "mandate sweep: issues change feed for $repo reached query_limit $query_limit; refusing a truncated sweep" >&2
    return 6
  fi
}

# #106/#109: sweep_org() below is the entire per-repository body -- every
# `gh` call the sweep makes. It runs once per distinct GitHub organisation
# in the roster, each time inside its own gh-as.sh invocation, so every
# organisation's repositories are read under a token minted for that
# organisation's own App installation.
#
# A GitHub App installation token is scoped to one installation, and one
# installation covers at most one organisation's repositories (a token
# minted against an installation on `onsager-ai` 404s every call against
# `crawlab-team`, regardless of which role minted it -- see #109). A single
# whole-run token, keyed off just the first configured repository, would
# silently 404 every repository in any other organisation the roster
# spans; each of those reads degrades to "no drift, no lead" exactly as a
# rate limit does, and the queue would quietly lose an entire
# organisation's rows while looking perfectly healthy -- the one outcome
# this whole sweep exists to prevent (#78).
#
# Minting per call would avoid that too, but multiplies round trips against
# GitHub's App endpoints by the number of `gh` calls the loop below makes,
# on a timer that already queries the whole roster. Minting per
# organisation is the actual unit a token is scoped to: it keeps the
# reasoning behind "not per call" (auth cost should scale with something
# far smaller than "every `gh` call") while fixing the earlier version's
# mistake of treating the whole roster as if it shared one installation.
# Auth cost now scales with organisation count, never repository count, and
# no organisation's repositories are ever read under a token that cannot
# see them.
#
# Identity: the gatekeeper's, not a new third App. A GitHub App installation
# token grants the same effective access regardless of which role minted
# it -- Claude Code's per-role deny lists are what actually separate builder
# from gatekeeper, and those apply only inside a harness-driven session, not
# to a freestanding script like this one. Introducing a third App would add
# a credential to hold and rotate without adding any enforcement a session
# doesn't already provide, and creating one is the principal's ceremony to
# run, not this change's. The gatekeeper is also already the role that
# authenticates and reads every open pull request across the whole roster
# (see skills/gatekeep/SKILL.md step 5) -- this sweep is the read-only
# counterpart of exactly that pattern, just triggered on a timer instead of
# on demand. If `gatekeeper` credentials are not configured, this fails
# below with a visible fault; it never falls back to an ambient token, and
# a failure for any single organisation aborts the whole sweep before any
# queue write -- it is never treated as an empty result for that
# organisation alone, the same way an issue/PR query failure for any single
# repository always has been.
sweep_org() {
  local org="$1"

  while IFS= read -r project; do
    printf '%s\n' "$project" >"$work/project.json"
    repo="$(jq -r '.repo' "$work/project.json")"
    gh_error="$work/gh-error"
    jq -c --arg repo "$repo" '.repos[$repo] // {}' "$work/old-state.json" \
      >"$work/previous.json"
    previous_cursor="$(jq -r '.cursor // ""' "$work/previous.json")"
    previous_etag="$(jq -r '.etag // ""' "$work/previous.json")"

    issue_since=""
    if [ "$sweep_mode" = "incremental" ]; then
      issue_since="$previous_cursor"
    fi
    fetch_issues "$repo" "$issue_since" "$previous_etag" || exit $?
    issue_cursor_candidate="$(
      jq -r '[.[] | (.updatedAt // .updated_at // empty)] | max // ""' \
        "$work/issues.json"
    )"
    # Pull requests intentionally stay on one complete listing. Check and file
    # changes do not reliably move updatedAt, so putting open PRs behind the
    # issues change-feed cursor would leave stale CI and path classifications.
    # The same response also carries merged PRs for the gate-invariant join
    # below. This changes no call count: one all-state listing replaces the
    # former open-only listing, then the two populations are split locally.
    if ! gh pr list --repo "$repo" --state all --limit "$query_limit" \
      --json number,title,body,labels,createdAt,updatedAt,url,isDraft,reviewDecision,statusCheckRollup,closingIssuesReferences,files,state,mergedAt,headRefOid \
      >"$work/all-prs.json" 2>"$gh_error"; then
      detail="$(tr '\n' ' ' <"$gh_error")"
      echo "mandate sweep: failed to query PRs, CI, and merged heads for $repo${detail:+: $detail}" >&2
      exit 5
    fi
    if [ "$(jq 'length' "$work/all-prs.json")" -eq "$query_limit" ]; then
      echo "mandate sweep: PR query for $repo reached query_limit $query_limit; refusing a truncated sweep" >&2
      exit 6
    fi
    jq -c '[.[] | select(
      .state == "OPEN"
      or ((has("state") | not) and (.mergedAt // null) == null)
    )]' "$work/all-prs.json" >"$work/prs.json"
    jq -c '[.[] | select(
      .state == "MERGED"
      or ((.mergedAt // "") | type == "string" and length > 0)
    )]' "$work/all-prs.json" >"$work/merged-prs.json"
    item_cap='null'

    # #78/#77 both need the default branch's own history — one lookup here,
    # not two. Unlike the issue/PR queries above, a failure here degrades to
    # "no drift row, no landed-fix lead" rather than aborting the sweep: this
    # data augments the queue, it does not gate whether the sweep can run at
    # all, and a rate-limited follow-up call must never take the whole queue
    # down with it.
    default_branch=""
    if gh repo view "$repo" --json defaultBranchRef >"$work/repo-view.json" 2>"$gh_error"; then
      default_branch="$(jq -r '.defaultBranchRef.name // ""' "$work/repo-view.json")"
    else
      detail="$(tr '\n' ' ' <"$gh_error")"
      echo "mandate sweep: failed to read the default branch for $repo${detail:+: $detail}; skipping CI drift and landed-fix checks this sweep" >&2
    fi

    # One run-list call per repo, same shape as the issue/PR queries above.
    # Judged later against only the LATEST run per workflow on this ref — an
    # older failure a later run turned green must not read as drift.
    printf '%s\n' '[]' >"$work/ci-runs.json"
    if [ -n "$default_branch" ]; then
      if ! gh run list --repo "$repo" --branch "$default_branch" --limit "$query_limit" \
        --json databaseId,workflowDatabaseId,workflowName,name,headSha,conclusion,status,createdAt,url \
        >"$work/ci-runs.json" 2>"$gh_error"; then
        detail="$(tr '\n' ' ' <"$gh_error")"
        echo "mandate sweep: failed to query default-branch CI runs for $repo${detail:+: $detail}; no drift row this sweep" >&2
        printf '%s\n' '[]' >"$work/ci-runs.json"
      fi
    fi

    jq -cn \
      --arg repo "$repo" \
      --slurpfile issues "$work/issues.json" \
      --slurpfile prs "$work/prs.json" '
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
        def dependency_refs($repo; $text):
          [
            $text
            | match(
                "(?:depends[[:space:]]+on|blocked[[:space:]]+by|gate[[:space:]]+for)[[:space:]]+((?:[[:alnum:]_.-]+/[[:alnum:]_.-]+)?#[1-9][0-9]*)";
                "ig"
              )
            | .captures[0].string
            | if startswith("#") then $repo + . else . end
          ]
          | unique;
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
              blocked_by: dependency_refs($repo; (.body // "")),
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
        [
          ($issues[0][]
            | select((.pull_request // null) == null)
            | .createdAt = (.createdAt // .created_at)
            | .updatedAt = (.updatedAt // .updated_at)
            | .url = (.url // .html_url)
            | normalized("issue")),
          ($prs[0][] | normalized("pr"))
        ]
      ' >"$work/fresh-items.json"

    if [ "$sweep_mode" = "full" ] && [ "$issue_http_status" != "304" ]; then
      cp "$work/fresh-items.json" "$work/items.json"
    else
      # The issues response is a delta, while the PR response is a complete
      # open set. Retain untouched issues, replace every PR, then overlay all
      # fresh records by id. The same merge is safe for a conditional full
      # request that returned 304 because its complete open representation is
      # unchanged. A later non-304 full reconciliation is what removes an
      # issue that left the open set without appearing in `since` results.
      jq -cn \
        --slurpfile previous "$work/previous.json" \
        --slurpfile fresh "$work/fresh-items.json" '
          ([($previous[0].records // {})[] | select(.type != "pr")]) as $issues
          | reduce $fresh[0][] as $item ($issues;
              map(select(.id != $item.id)) + [$item]
            )
          | sort_by(.id)
        ' >"$work/items.json"
    fi

    jq -cn \
      --slurpfile project "$work/project.json" \
      --slurpfile config "$work/config.json" '
        ($project[0]) as $project
        | ($config[0].bounce_all) as $bounce_all
        |
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
      ' >"$work/policy.json"

    jq -cn \
        --arg sweep_started "$sweep_started" \
        --arg sweep_mode "$sweep_mode" \
        --arg issue_cursor_candidate "$issue_cursor_candidate" \
        --slurpfile project "$work/project.json" \
        --slurpfile config "$work/config.json" \
        --slurpfile items "$work/items.json" \
        --slurpfile policy "$work/policy.json" \
        --argjson item_cap "$item_cap" \
        --slurpfile previous "$work/previous.json" '
        ($project[0]) as $project
        | ($config[0]) as $config
        | ($items[0]) as $items
        | ($policy[0]) as $policy
        | ($previous[0]) as $previous
        |
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
        # Architectural convention only; nothing enforces it. Mandate-side code
        # may reuse the constitution escalation-dossier shape, but constitution-side
        # code must never learn about mandates, queues, grants, or GitHub. Keeping
        # this direction one-way preserves an independently usable rule system.
        def mandate_record($item; $kind; $reason):
          if $kind == "tripwire" then
            {
              reason: $reason,
              dossier: {
                question: ("May " + $item.repo + $item.ref + " cross the matched mandate tripwire?"),
                options_ruled_out: [
                  "Auto-proceed — a tripwire requires human judgment."
                ],
                recommended_action: ("Review " + $item.repo + $item.ref + ", then approve, reject, or defer it in /ostrom:desk."),
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
            | (
                (
                  (($sweep_started | fromdateiso8601) - ($item.opened | fromdateiso8601))
                  / 86400
                  | floor
                ) as $days
                | [$days, 0]
                | max
              ) as $age_days
            | . + {
                classification: $classification,
                first_seen: $first_seen,
                age_days: $age_days,
                movement_stuck: (
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
                or ($item.movement_stuck and (($item.old.stuck // false) | not))
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
                  or (
                    ($project.paused | not)
                    and ($item.classification.terminal | IN("delegated", "unclassified"))
                  )
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
                elif $item.classification.terminal == "unclassified" then
                  {
                    kind: "decision",
                    reason: (
                      "no selector matched ("
                      + match_reason($item.classification)
                      + "); classification needed"
                    )
                  }
                elif $item.type == "pr" and $item.ci == "failing" then
                  {
                    kind: "drift",
                    reason: (
                      "CI is failing; "
                      + match_reason($item.classification)
                    )
                  }
                elif $item.movement_stuck then
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
                    reason: (
                      match_reason($item.classification)
                      + "; updated since the read cursor"
                    )
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
                opened: $item.opened,
                age_days: $item.age_days,
                aged_out: ($item.age_days >= $config.stuck_after_days),
                needs_judgment: ($row.kind | IN("tripwire", "decision")),
                blocked_by: $item.blocked_by
              }
          ]) as $rows
        | ([$visible_active[] | .id]) as $active_ids
        | (
            if $policy_changed and ($initial | not) then
              [
                $classified[]
                | . as $item
                | (
                    $item.old == null
                    or $item.old.fingerprint != $item.fingerprint
                    or $item.updated > ($previous.cursor // "")
                    or ($item.movement_stuck and (($item.old.stuck // false) | not))
                  ) as $event
                | (
                    $item.classification.terminal == "reserved"
                    or $item.classification.terminal == "tripwire"
                    or ($item.type == "pr" and $item.ci == "failing")
                  ) as $safety
                | select(
                    $event
                    and ($safety | not)
                    and ($project.paused | not)
                    and $item.classification.terminal == "delegated"
                  )
              ] | length
            else 0
            end
          ) as $suppressed_delegated
        | (reduce $classified[] as $item ({};
            .[$item.id] = {
              updated: $item.updated,
              fingerprint: $item.fingerprint,
              first_seen: $item.first_seen,
              classification: $item.classification.terminal,
              matched_selector: $item.classification.selector,
              stuck: $item.movement_stuck
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
            if $initial then $sweep_started
            elif $policy_changed then $previous.cursor
            elif $sweep_mode == "full" then $sweep_started
            else (
              [
                $previous.cursor,
                (if $issue_cursor_candidate > $sweep_started
                  then $sweep_started
                  else $issue_cursor_candidate
                  end)
              ]
              | max
            )
            end
          ) as $cursor
        | {
            rows: $rows,
            suppressed_delegated: $suppressed_delegated,
            active_ids: $active_ids,
            current_items: [
              $classified[]
              | . as $item
              | {
                  id: .id,
                  title: .title,
                  closing_suffix: closing_suffix($item; $active),
                  age_days: .age_days,
                  aged_out: (.age_days >= $config.stuck_after_days),
                  blocked_by: .blocked_by
                }
            ],
            selector_stats: [selector_stats],
            # #77 candidates: open issues about to read (or already reading)
            # as "stuck". Kept separate from $rows because the landed-fix
            # lookup below must run every sweep an issue sits stuck, not only
            # the sweep that first classifies it that way.
            stuck_issue_candidates: [
              $classified[]
              | select(.type == "issue" and .movement_stuck)
              | {number: .number, opened: .opened}
            ],
            repo_state: {
              cursor: $cursor,
              previous_cursor: (
                if $changed or $cursor != ($previous.cursor // null)
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
      ' >"$work/analysis.json"

    # #147: a merge with no timely passing verdict is a fault observed after
    # the fact. The all-state PR response above, gate.jsonl, and
    # exceptions.jsonl are already in hand; this is deliberately only a local
    # join. Faults use ordinary decision rows, so they can be explained,
    # deferred, or otherwise handled without changing merge behaviour.
    jq -cn \
      --arg repo "$repo" \
      --arg sweep_started "$sweep_started" \
      --argjson stuck_after_days "$(jq '.stuck_after_days' "$work/config.json")" \
      --slurpfile merged "$work/merged-prs.json" \
      --slurpfile gate "$work/gate-records.json" \
      --slurpfile exceptions "$work/exception-records.json" \
      --slurpfile previous "$work/previous.json" '
      def timestamp_before($candidate; $boundary):
        try (($candidate | fromdateiso8601) < ($boundary | fromdateiso8601))
        catch false;
      def age_days($opened):
        try (
          ((($sweep_started | fromdateiso8601) - ($opened | fromdateiso8601)) / 86400 | floor)
          | [., 0] | max
        ) catch 0;
      def reason($fault):
        if $fault.shape == "no_verdict" then
          "merge gate fault: no verdict for merged head " + $fault.head_sha
        elif $fault.shape == "non_pass" then
          "merge gate fault: " + $fault.verdict
          + " verdict for merged head " + $fault.head_sha
        else
          "merge gate fault: pass recorded after merge for head " + $fault.head_sha
        end;
      (reduce $gate[0][] as $record ({};
        if (($record.head_sha // null) | type) == "string"
            and ($record.head_sha | length) > 0
        then .[$record.head_sha] += [$record]
        else .
        end
      )) as $gate_by_sha
      | ($previous[0].merge_gate_merges // {}) as $known_merges
      | (reduce $merged[0][] as $pr ($known_merges;
          select(($pr.number | type) == "number")
          | ($repo + "#" + ($pr.number | tostring)) as $id
          | .[$id] = {
              id: $id,
              number: $pr.number,
              title: (
                ($pr.title // "")
                | if type == "string" and length > 0
                  then . else "(title unavailable)" end
              ),
              created_at: ($pr.createdAt // $pr.mergedAt // $sweep_started),
              merged_at: ($pr.mergedAt // ""),
              head_sha: ($pr.headRefOid // "")
            }
        )) as $merges
      | [
          $merges[]
          | select((.merged_at | type) == "string" and (.merged_at | length) > 0)
          | . as $merge
          | ($gate_by_sha[$merge.head_sha] // []) as $records
          | ([$records[] | select(
              .verdict == "pass"
              and timestamp_before((.ts // ""); $merge.merged_at)
            )]) as $timely_passes
          | ([$records[] | select(.verdict == "pass")]) as $passes
          | (
              if ($timely_passes | length) > 0 then null
              elif ($records | length) == 0 then {
                shape: "no_verdict",
                verdict: "none",
                gate_ts: null
              }
              elif ($passes | length) > 0 then {
                shape: "pass_after_merge",
                verdict: "pass",
                gate_ts: ($passes[0].ts // null)
              }
              else {
                shape: "non_pass",
                verdict: (($records | last).verdict // "inconclusive"),
                gate_ts: (($records | last).ts // null)
              }
              end
            ) as $violation
          | select($violation != null)
          | ([
              $exceptions[0][]
              | select(
                  .repo == $repo
                  and .pr == $merge.number
                  and .head_sha == $merge.head_sha
                  and .condition == "merge_protocol"
                  and ((.reason // null) | type) == "string"
                )
            ] | last // null) as $exception
          | $merge + $violation + {
              exception: $exception,
              fingerprint: ([$violation.shape, $merge.head_sha,
                $violation.verdict, ($violation.gate_ts // "")] | join("|"))
            }
        ] as $violations
      | ([$violations[] | select(.exception == null)]) as $faults
      | ([$violations[] | select(.exception != null)]) as $excused
      | ($previous[0].merge_gate_faults // {}) as $old_faults
      | {
          rows: [
            $faults[]
            | select(($old_faults[.id].fingerprint // "") != .fingerprint)
            | . as $fault
            | (age_days($fault.merged_at)) as $age
            | {
                id: $fault.id,
                repo: $repo,
                ref: ("#" + ($fault.number | tostring)),
                title: $fault.title,
                kind: "decision",
                mandate: {reason: reason($fault)},
                state: "pending",
                opened: $fault.merged_at,
                age_days: $age,
                aged_out: ($age >= $stuck_after_days),
                needs_judgment: true,
                blocked_by: []
              }
          ],
          active_ids: [$faults[].id],
          current_items: [
            $faults[]
            | (age_days(.merged_at)) as $age
            | {
                id,
                title,
                age_days: $age,
                aged_out: ($age >= $stuck_after_days)
              }
          ],
          merges: $merges,
          faults: (reduce $faults[] as $fault ({};
            .[$fault.id] = {
              shape: $fault.shape,
              head_sha: $fault.head_sha,
              verdict: $fault.verdict,
              gate_ts: $fault.gate_ts,
              fingerprint: $fault.fingerprint
            }
          )),
          excuses: (reduce $excused[] as $item ({};
            .[$item.id] = {
              head_sha: $item.head_sha,
              reason: $item.exception.reason
            }
          )),
          fault_count: ($faults | length)
        }
      ' >"$work/merge-gate.json"

    # #78: fold the default branch's own CI into a "drift" row, the same kind
    # and digest/troubled-project machinery an open PR's failing CI already
    # uses. Regeneration is gated on .event the same way the rest of this
    # script gates $rows on $event: a still-red workflow with no new run is
    # carried forward via active_ids rather than rewritten every sweep, so an
    # /ostrom:desk approval on it survives until the run actually changes.
    jq -cn \
      --arg repo "$repo" \
      --arg sweep_started "$sweep_started" \
      --argjson stuck_after_days "$(jq '.stuck_after_days' "$work/config.json")" \
      --slurpfile runs "$work/ci-runs.json" \
      --slurpfile old_state "$work/old-state.json" '
      def failure_conclusion:
        ((. // "") | ascii_upcase)
        | IN("FAILURE", "CANCELLED", "TIMED_OUT", "ACTION_REQUIRED", "STARTUP_FAILURE");
      ($old_state[0].repos[$repo].ci_drift // {}) as $previous_ci
      | (($old_state[0].repos[$repo].cursor // null) == null) as $initial
      | ($runs[0] | sort_by(.createdAt) | reverse) as $runs_desc
      | ($runs_desc | group_by(.workflowDatabaseId // .name // "")) as $groups
      | [
          $groups[]
          | (sort_by(.createdAt) | reverse) as $history
          | $history[0] as $latest
          # A run still in flight has no verdict yet; skip it this sweep
          # rather than judging a workflow on data it has not produced.
          | select($latest.status == "completed")
          | select($latest.conclusion | failure_conclusion)
          # Org/enterprise ruleset workflows can report with no workflow id
          # (see `gh run list --help`). Without a numeric id there is no way
          # to mint a stable ref that also satisfies the queue schema, so the
          # workflow is skipped rather than guessed at.
          | select(($latest.workflowDatabaseId | type) == "number")
          | (
              reduce $history[] as $run (
                {done: false, red_since: $latest.createdAt};
                if .done then .
                elif ($run.status != "completed") then .
                elif ($run.conclusion | failure_conclusion) then .red_since = $run.createdAt
                else .done = true
                end
              )
            ) as $streak
          | ($latest.workflowDatabaseId | tostring) as $wf_key
          | ($previous_ci[$wf_key] // null) as $old
          | (
              if ($old != null) and ($old.red_since < $streak.red_since)
              then $old.red_since
              else $streak.red_since
              end
            ) as $red_since
          | {
              workflow_id: $latest.workflowDatabaseId,
              workflow_name: (
                ($latest.workflowName // $latest.name // "")
                | if length > 0 then . else "(workflow name unavailable)" end
              ),
              run_id: $latest.databaseId,
              head_sha: ($latest.headSha // ""),
              red_since: $red_since,
              event: ($initial or $old == null or $old.run_id != $latest.databaseId)
            }
        ] as $failing
      |
      # A row not regenerated this sweep (no event) is still carried forward by
      # id via active_ids below, but carrying it forward must not leave its
      # age_days frozen at whatever sweep last rewrote it — that is the exact
      # kind of stale queue fact this pair of fixes exists to remove. Compute
      # age fresh for every currently-failing workflow, same as current_items
      # already does for issues and PRs, so enrich() can refresh it below
      # whether or not this sweep regenerated the row.
      def with_age:
        . as $item
        | (($sweep_started | fromdateiso8601) - ($item.red_since | fromdateiso8601)) as $age_seconds
        | ([($age_seconds / 86400 | floor), 0] | max) as $age_days
        | $item + {age_days: $age_days, aged_out: ($age_days >= $stuck_after_days)};
      ($failing | map(with_age)) as $failing_aged
      | {
          rows: [
            $failing_aged[]
            | select(.event)
            | {
                id: ($repo + "#" + (.workflow_id | tostring)),
                repo: $repo,
                ref: ("#" + (.workflow_id | tostring)),
                title: ("CI failing on default branch: " + .workflow_name),
                kind: "drift",
                mandate: {
                  reason: (
                    "default branch CI failing: " + .workflow_name
                    + "; run " + (.run_id | tostring)
                    + " at " + (.head_sha[0:8])
                    + "; red since " + .red_since
                  )
                },
                state: "pending",
                opened: .red_since,
                age_days: .age_days,
                aged_out: .aged_out,
                needs_judgment: false,
                blocked_by: []
              }
          ],
          active_ids: [$failing[] | ($repo + "#" + (.workflow_id | tostring))],
          current_items: [
            $failing_aged[]
            | {
                id: ($repo + "#" + (.workflow_id | tostring)),
                age_days: .age_days,
                aged_out: .aged_out
              }
          ],
          state: (
            reduce $failing[] as $item ({};
              .[($item.workflow_id | tostring)] = {
                run_id: $item.run_id,
                red_since: $item.red_since
              }
            )
          )
        }
      ' >"$work/ci-drift.json"

    # #77: for every issue about to read (or still reading) as "stuck", look
    # for a default-branch commit naming it without a closing keyword. One
    # call per candidate, capped at the issue's own opened date — never one
    # call per open issue in the repo.
    rm -f "$work/candidate-result.jsonl"
    printf '%s\n' '[]' >"$work/repo-possibly-landed.json"
    if [ -n "$default_branch" ]; then
      while IFS=$'\t' read -r cand_number cand_opened; do
        [ -n "$cand_number" ] || continue
        commit_error="$work/gh-commit-error"
        printf '.\n' >>"$work/landed-fix-attempts.count"
        if gh api -X GET "repos/$repo/commits" \
            -f "sha=$default_branch" \
            -f "since=$cand_opened" \
            -f "per_page=100" \
            --jq '[.[] | {sha: .sha, message: .commit.message, date: (.commit.committer.date // .commit.author.date // "")}]' \
            >"$work/candidate-commits.json" 2>"$commit_error"; then
          jq -c \
            --arg id "$repo#$cand_number" \
            --argjson number "$cand_number" \
            --arg opened "$cand_opened" '
            def closing_count($msg; $n):
              [$msg | scan("\\b(?:close[sd]?|fix(?:e[sd])?|resolve[sd]?)[[:space:]:]*#" + ($n|tostring) + "(?!\\d)"; "i")]
              | length;
            def bare_count($msg; $n):
              [$msg | scan("#" + ($n|tostring) + "(?!\\d)"; "i")] | length;
            (.) as $commits
            | [
                $commits[]
                | select((.date // "") >= $opened)
                | select(bare_count(.message; $number) > closing_count(.message; $number))
              ] as $bare_matches
            | if ($bare_matches | length) == 0 then empty
              else
                ($bare_matches | sort_by(.date) | .[0]) as $earliest
                | {
                    id: $id,
                    possibly_landed: (
                      "; possibly landed: " + ($earliest.sha[0:8])
                      + " references #" + ($number | tostring)
                      + " without a closing keyword"
                    )
                  }
              end
            ' "$work/candidate-commits.json" >>"$work/candidate-result.jsonl"
        else
          detail="$(tr '\n' ' ' <"$commit_error")"
          printf '%s' "$detail" >"$work/landed-fix-last-error.txt"
          echo "mandate sweep: failed to search default-branch commits for $repo#$cand_number${detail:+: $detail}; no landed-fix lead this sweep" >>"$work/landed-fix-failures.log"
        fi
      done < <(jq -r '.stuck_issue_candidates[]? | [(.number|tostring), .opened] | @tsv' "$work/analysis.json")
      if [ -s "$work/candidate-result.jsonl" ]; then
        jq -cs '.' "$work/candidate-result.jsonl" >"$work/repo-possibly-landed.json"
      fi
    fi

    jq -cn \
      --slurpfile all "$work/generated.json" \
      --slurpfile analysis "$work/analysis.json" \
      --slurpfile merge_gate "$work/merge-gate.json" \
      --slurpfile ci_drift "$work/ci-drift.json" \
      '$all[0] + $analysis[0].rows + $merge_gate[0].rows + $ci_drift[0].rows' >"$work/next.json"
    mv "$work/next.json" "$work/generated.json"
    jq -cn \
      --slurpfile all "$work/active-ids.json" \
      --slurpfile analysis "$work/analysis.json" \
      --slurpfile merge_gate "$work/merge-gate.json" \
      --slurpfile ci_drift "$work/ci-drift.json" \
      '$all[0] + $analysis[0].active_ids + $merge_gate[0].active_ids + $ci_drift[0].active_ids' >"$work/next.json"
    mv "$work/next.json" "$work/active-ids.json"

    # An approval is deliberately durable across an item disappearing from
    # the open enumeration: absence can be an API or rate-limit failure, not
    # proof that the work is done. For approved rows that would otherwise be
    # carried over, make one positive state read instead. A merged PR and a
    # closed-unmerged PR are recorded distinctly, although both are terminal
    # for queue purposes. Any command failure or malformed/incomplete response
    # records nothing, so the final reconciliation retains the approval.
    while IFS=$'\t' read -r approved_id approved_number; do
      if gh pr view "$approved_number" --repo "$repo" --json state,mergedAt \
          >"$work/approved-pr-state.json" 2>"$work/approved-pr-state-error"; then
        if approved_resolution="$(
          jq -er '
            if type != "object"
                or (has("state") | not)
                or (has("mergedAt") | not)
            then error("incomplete pull request state")
            elif .state == "MERGED"
                and (.mergedAt | type) == "string"
                and (.mergedAt | length) > 0
            then "merged"
            elif .state == "CLOSED" and .mergedAt == null
            then "closed-unmerged"
            elif .state == "OPEN" and .mergedAt == null
            then "open"
            else error("unrecognized pull request state")
            end
          ' "$work/approved-pr-state.json" 2>/dev/null
        )"; then
          case "$approved_resolution" in
            merged | closed-unmerged)
              jq -cn \
                --arg id "$approved_id" \
                --arg resolution "$approved_resolution" \
                '{id: $id, resolution: $resolution}' \
                >>"$work/closed-approved.jsonl"
              ;;
          esac
        fi
      fi
    done < <(
      jq -r \
        --arg repo "$repo" \
        --slurpfile active_ids "$work/active-ids.json" '
        .[]
        | . as $row
        | select(.state == "approved" and .repo == $repo)
        | select(($active_ids[0] | index($row.id)) == null)
        | select(.id | startswith($repo + "#"))
        | (.id | split("#")[-1]) as $number
        | select($number | test("^[1-9][0-9]*$"))
        | [.id, $number]
        | @tsv
      ' "$work/existing-queue.json"
    )
    jq -cn \
      --slurpfile all "$work/current-items.json" \
      --slurpfile analysis "$work/analysis.json" \
      --slurpfile merge_gate "$work/merge-gate.json" \
      --slurpfile ci_drift "$work/ci-drift.json" \
      '$all[0] + $analysis[0].current_items + $merge_gate[0].current_items + $ci_drift[0].current_items' >"$work/next.json"
    mv "$work/next.json" "$work/current-items.json"
    jq -cn \
      --slurpfile all "$work/selector-stats.json" \
      --slurpfile analysis "$work/analysis.json" \
      '$all[0] + $analysis[0].selector_stats' >"$work/next.json"
    mv "$work/next.json" "$work/selector-stats.json"
    jq -r '.suppressed_delegated' "$work/analysis.json" \
      >>"$work/policy-suppressed.count"
    jq -cn \
      --slurpfile all "$work/possibly-landed.json" \
      --slurpfile repo "$work/repo-possibly-landed.json" \
      '$all[0] + $repo[0]' >"$work/next.json"
    mv "$work/next.json" "$work/possibly-landed.json"
    jq -cn \
        --arg repo "$repo" \
        --slurpfile state "$work/new-state.json" \
        --slurpfile analysis "$work/analysis.json" \
        --slurpfile merge_gate "$work/merge-gate.json" \
        --slurpfile ci_drift "$work/ci-drift.json" \
        --slurpfile records "$work/items.json" \
        --arg etag "${issue_etag:-$previous_etag}" \
        '$state[0] | .version = 2
          | .repos[$repo] = (
              $analysis[0].repo_state
              + {
                  ci_drift: $ci_drift[0].state,
                  merge_gate_merges: $merge_gate[0].merges,
                  merge_gate_faults: $merge_gate[0].faults,
                  merge_gate_excuses: $merge_gate[0].excuses,
                  merge_gate_fault_count: $merge_gate[0].fault_count,
                  etag: (if $etag == "" then null else $etag end),
                  records: (reduce $records[0][] as $record ({}; .[$record.id] = $record))
                }
            )' \
        >"$work/next.json"
    mv "$work/next.json" "$work/new-state.json"
  done < <(jq -c --arg org "$org" '.projects[] | select((.repo | split("/")[0]) == $org)' "$work/config.json")
}

if [ -n "${MANDATE_SWEEP_ORG:-}" ]; then
  # Inner/per-organisation mode. MANDATE_SWEEP_WORK and MANDATE_SWEEP_TIME
  # were set by the dispatch loop below, and this process is already
  # running under a gh-as.sh-minted token scoped to MANDATE_SWEEP_ORG's
  # installation by the time it starts. Confirm that token actually works,
  # then process just this organisation's repositories.
  work="$MANDATE_SWEEP_WORK"

  gh_host="${GH_HOST:-github.com}"
  if ! gh auth status --hostname "$gh_host" >/dev/null 2>&1; then
    echo "mandate sweep: gh reports not authenticated for $gh_host using the freshly minted gatekeeper token for $MANDATE_SWEEP_ORG; the sweep never falls back to an ambient credential" >&2
    exit 3
  fi

  sweep_started="${MANDATE_SWEEP_TIME:-$(date -u +%Y-%m-%dT%H:%M:%SZ)}"
  sweep_mode="$MANDATE_SWEEP_MODE_EFFECTIVE"
  sweep_org "$MANDATE_SWEEP_ORG"
  exit 0
fi

# Outer/driver mode: one-time setup, then one authenticated subprocess per
# organisation in the roster (see sweep_org's own comment above for why),
# then the shared tail -- dead-selector accounting, the final queue write,
# and publish. The driver itself never calls `gh` and never holds a token.
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

mandate_load_config >"$work/config.json" || exit
project_count="$(jq '.projects | length' "$work/config.json")"
if [ "$project_count" -eq 0 ]; then
  echo "mandate sweep: mandates.yaml contains no projects" >&2
  exit 2
fi

mkdir -p "$MANDATE_DATA_DIR"
mandate_read_queue >"$work/existing-queue.json" || {
  echo "mandate sweep: cannot read $MANDATE_QUEUE_FILE" >&2
  exit 4
}
if [ -s "$MANDATE_STATE_FILE" ]; then
  if ! jq -c 'if type == "object" then . else error("state is not an object") end' \
    "$MANDATE_STATE_FILE" >"$work/old-state.json"; then
    echo "mandate sweep: cannot read $MANDATE_STATE_FILE" >&2
    exit 4
  fi
else
  printf '%s\n' '{"version":2,"repos":{},"dead_selectors":[]}' \
    >"$work/old-state.json"
fi

sweep_started="${MANDATE_SWEEP_TIME:-$(date -u +%Y-%m-%dT%H:%M:%SZ)}"
requested_sweep_mode="${MANDATE_SWEEP_MODE:-auto}"
case "$requested_sweep_mode" in
  auto | full | incremental) ;;
  *)
    echo "mandate sweep: MANDATE_SWEEP_MODE must be auto, full, or incremental" >&2
    exit 2
    ;;
esac

state_supports_incremental="$(
  jq -r \
    --slurpfile config "$work/config.json" '
      . as $state
      | all($config[0].projects[];
          .repo as $repo
          | ($state.repos[$repo].cursor | type) == "string"
          and ($state.repos[$repo].records | type) == "object"
        )
    ' "$work/old-state.json"
)"
full_reconciliation_due="$(
  jq -nr \
    --arg now "$sweep_started" \
    --arg last "$(jq -r '.last_full_reconciliation // ""' "$work/old-state.json")" \
    --argjson interval "$full_reconciliation_seconds" '
      try (
        $last == ""
        or ((($now | fromdateiso8601) - ($last | fromdateiso8601)) as $age
          | $age < 0 or $age >= $interval)
      ) catch true
    '
)"
if [ "$requested_sweep_mode" = "full" ] || \
  [ "$state_supports_incremental" != "true" ] || \
  { [ "$requested_sweep_mode" = "auto" ] && [ "$full_reconciliation_due" = "true" ]; }; then
  sweep_mode="full"
else
  sweep_mode="incremental"
fi
printf '%s\n' '[]' >"$work/generated.json"
printf '%s\n' '[]' >"$work/active-ids.json"
printf '%s\n' '[]' >"$work/current-items.json"
printf '%s\n' '[]' >"$work/selector-stats.json"
: >"$work/policy-suppressed.count"
# Leads for #77's already-landed-but-not-closed check, keyed by issue id
# ("owner/repo#N"), accumulated across repos the same way the lists above are.
printf '%s\n' '[]' >"$work/possibly-landed.json"
cp "$work/old-state.json" "$work/new-state.json"

# Snapshot the append-only evidence once for every organisation subprocess.
# Missing or empty logs are legitimate first-run states. Malformed evidence is
# reported but cannot turn this detective check into a failed sweep; treating
# unreadable verdicts as absent keeps the possible faults visible.
printf '%s\n' '[]' >"$work/gate-records.json"
if [ -s "$MANDATE_GATE_LOG" ]; then
  if ! jq -s '[.[] | select(type == "object")]' "$MANDATE_GATE_LOG" \
      >"$work/gate-records.json" 2>/dev/null; then
    echo "mandate sweep: could not read $MANDATE_GATE_LOG; merge gate faults will be classified as having no verdict" >&2
    printf '%s\n' '[]' >"$work/gate-records.json"
  fi
fi
printf '%s\n' '[]' >"$work/exception-records.json"
if [ -s "$MANDATE_EXCEPTIONS_LOG" ]; then
  if ! jq -s '[.[] | select(type == "object")]' "$MANDATE_EXCEPTIONS_LOG" \
      >"$work/exception-records.json" 2>/dev/null; then
    echo "mandate sweep: could not read $MANDATE_EXCEPTIONS_LOG; ignoring merge gate exceptions" >&2
    printf '%s\n' '[]' >"$work/exception-records.json"
  fi
fi

# #86: track landed-fix commit-search attempts/failures across the whole
# sweep (all organisations, all repos, all candidates) so a 100%-failing
# capability can be reported once instead of once per candidate. Per-item
# lines are buffered and only flushed verbatim when the failure is partial
# (i.e. transient); see the summary after the dispatch loop below. These are
# files, not shell counters: sweep_org() for a given organisation runs in a
# separate process (the gh-as.sh-wrapped child below), so a plain shell
# variable it incremented would never be visible back here.
: >"$work/landed-fix-attempts.count"
: >"$work/landed-fix-failures.log"
: >"$work/landed-fix-last-error.txt"
# Positive terminal observations for carried-over approvals. This is JSONL
# because each authenticated per-organisation subprocess appends its own
# observations; an empty file means every absent approval remains durable.
: >"$work/closed-approved.jsonl"

while IFS= read -r org; do
  anchor_repo="$(
    jq -r --arg org "$org" \
      'first(.projects[] | select((.repo | split("/")[0]) == $org) | .repo)' \
      "$work/config.json"
  )"
  # anchor_repo only tells gh-as.sh/app-token.sh which installation to
  # resolve a token for; sweep_org (invoked as this exact process, under
  # that token) then reads every repository under $org, not just the
  # anchor. MANDATE_SWEEP_TIME carries the one sweep_started timestamp this
  # driver already computed, so every organisation's rows are classified
  # against the same instant regardless of which one is processed first.
  MANDATE_SWEEP_ORG="$org" MANDATE_SWEEP_WORK="$work" MANDATE_SWEEP_TIME="$sweep_started" \
    MANDATE_SWEEP_MODE_EFFECTIVE="$sweep_mode" \
    bash "$SCRIPT_DIR/gh-as.sh" gatekeeper "$anchor_repo" \
    bash "${BASH_SOURCE[0]}"
done < <(jq -r '[.projects[].repo | split("/")[0]] | unique | .[]' "$work/config.json")

policy_suppressed="$(jq -s 'add // 0' "$work/policy-suppressed.count")"

landed_fix_attempts="$(wc -l <"$work/landed-fix-attempts.count" | tr -d '[:space:]')"
landed_fix_failures="$(wc -l <"$work/landed-fix-failures.log" | tr -d '[:space:]')"
landed_fix_last_error="$(cat "$work/landed-fix-last-error.txt")"

# #86: report the landed-fix commit search as one capability-level failure
# when every attempt this sweep failed (a dead capability), or as the
# per-item lines buffered above when only some attempts failed (transient).
# A sweep with zero candidates has nothing to report either way.
if [ "$landed_fix_attempts" -gt 0 ] && [ "$landed_fix_failures" -eq "$landed_fix_attempts" ]; then
  echo "mandate sweep: default-branch commit search failed for all $landed_fix_attempts stuck-issue candidate(s) this sweep; landed-fix lead unavailable this sweep, not just degraded${landed_fix_last_error:+ (last error: $landed_fix_last_error)}" >&2
elif [ "$landed_fix_failures" -gt 0 ]; then
  cat "$work/landed-fix-failures.log" >&2
fi

jq -cn --slurpfile stats "$work/selector-stats.json" '
    $stats[0]
    | sort_by([(.repo // ""), .source, .selector])
    | group_by([.repo, .source, .selector])
    | map(
        select(any(.[]; .hit) | not)
        | first
        | del(.hit)
      )
  ' >"$work/dead-selectors.json"
jq -c '[.projects[].repo]' "$work/config.json" >"$work/configured-repos.json"
jq -cn \
    --slurpfile state "$work/new-state.json" \
    --slurpfile dead "$work/dead-selectors.json" \
    --slurpfile configured_repos "$work/configured-repos.json" '
    $state[0]
    | ($dead[0]) as $dead
    | ($configured_repos[0]) as $configured_repos
    | .dead_selectors = $dead
    | .repos |= with_entries(
        select(.key as $repo | ($configured_repos | index($repo)) != null)
      )
  ' >"$work/next.json"
mv "$work/next.json" "$work/new-state.json"

jq -cn \
    --slurpfile state "$work/new-state.json" \
    --arg mode "$sweep_mode" \
    --arg sweep_started "$sweep_started" '
    $state[0]
    | .sweep_mode = $mode
    | if $mode == "full"
      then .last_full_reconciliation = $sweep_started
      else .
      end
  ' >"$work/next.json"
mv "$work/next.json" "$work/new-state.json"

jq -cn \
    --slurpfile existing "$work/existing-queue.json" \
    --slurpfile generated "$work/generated.json" \
    --slurpfile active_ids "$work/active-ids.json" \
    --slurpfile current_items "$work/current-items.json" \
    --slurpfile possibly_landed "$work/possibly-landed.json" \
    --slurpfile closed_approved "$work/closed-approved.jsonl" '
    ($existing[0]) as $existing
    | ($generated[0]) as $generated
    | ($active_ids[0]) as $active_ids
    | ($current_items[0]) as $current_items
    | ($possibly_landed[0]) as $possibly_landed
    | ($closed_approved | map(.id)) as $closed_approved_ids
    |
    def dependency_refs($repo; $text):
      [
        $text
        | match(
            "(?:depends[[:space:]]+on|blocked[[:space:]]+by|gate[[:space:]]+for)[[:space:]]+((?:[[:alnum:]_.-]+/[[:alnum:]_.-]+)?#[1-9][0-9]*)";
            "ig"
          )
        | .captures[0].string
        | if startswith("#") then $repo + . else . end
      ]
      | unique;
    def current($id):
      first($current_items[] | select(.id == $id)) // null;
    def possibly_landed($id):
      first($possibly_landed[] | select(.id == $id) | .possibly_landed) // "";
    def enrich:
      . as $row
      | (current($row.id)) as $current
      | if .kind == "moved"
          and (
            (.mandate.reason // .mandate // "")
            | endswith("; updated since the read cursor")
            | not
          )
        then
          if (.mandate | type) == "object"
          then .mandate.reason += "; updated since the read cursor"
          else .mandate += "; updated since the read cursor"
          end
        else .
        end
      | .title = ($current.title // .title // "(title unavailable)")
      | if $current != null
        then
          .age_days = $current.age_days
          | .aged_out = $current.aged_out
        else .
        end
      | .needs_judgment = (.kind | IN("tripwire", "decision"))
      | .blocked_by = (
          (
            ($current.blocked_by // .blocked_by // [])
            + dependency_refs(
                .repo;
                (
                  (.mandate.reason // .mandate // "")
                  | if type == "string" then . else "" end
                )
              )
          )
          | unique
        )
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
        end
      # #77: a pointer, never a verdict. This only ever appends to the
      # reason string on a "stuck" row — it must not touch state or kind, so
      # a later reader cannot "improve" it into an auto-close. The builder
      # still verifies the diff and closes.
      | (possibly_landed(.id)) as $possibly_landed_suffix
      | if .kind == "stuck"
          and $possibly_landed_suffix != ""
          and (
            (.mandate.reason // .mandate // "")
            | endswith($possibly_landed_suffix)
            | not
          )
        then
          if (.mandate | type) == "object"
          then .mandate.reason += $possibly_landed_suffix
          else .mandate += $possibly_landed_suffix
          end
        else .
        end;
    (
      $existing
      | map(. as $row
        | select(
            ($active_ids | index($row.id)) != null
            or (
              $row.state == "approved"
              and ($closed_approved_ids | index($row.id)) == null
            )
          )
      )
    ) as $still_relevant
    | reduce $generated[] as $row ($still_relevant;
        map(select(.id != $row.id)) + [$row]
      )
    | map(enrich)
    | sort_by(.opened, .id)
  ' >"$work/final-queue.json"

queue_changes="$(
  jq -n \
    --slurpfile before "$work/existing-queue.json" \
    --slurpfile after "$work/final-queue.json" '
    ($before[0]) as $before
    | ($after[0]) as $after
    |
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

jq -c '.[]' "$work/final-queue.json" >"$work/queue.jsonl"
jq -S . "$work/new-state.json" >"$work/state.json"
mandate_write_if_changed "$work/queue.jsonl" "$MANDATE_QUEUE_FILE"
mandate_write_if_changed "$work/state.json" "$MANDATE_STATE_FILE"

# The state file mtime is the configured-cadence stamp. Touching it changes no
# serialized state, so a repeat sweep with no upstream activity has an empty
# content diff.
touch "$MANDATE_STATE_FILE"
if [ "$policy_suppressed" -gt 0 ]; then
  policy_suppressed_noun="delegated rows"
  [ "$policy_suppressed" -ne 1 ] || policy_suppressed_noun="delegated row"
  echo "mandate sweep: $project_count projects; $queue_changes queue changes; $policy_suppressed $policy_suppressed_noun suppressed by mandate change"
else
  echo "mandate sweep: $project_count projects; $queue_changes queue changes"
fi

# Publishing is downstream of the governing sweep. A config guard skip is a
# deliberate outcome distinct from publication failures; neither can change
# the sweep's successful outcome.
publish_status=0
bash "$SCRIPT_DIR/publish.sh" || publish_status=$?
case "$publish_status" in
  0) ;;
  3)
    echo "mandate sweep: publish deliberately skipped by config guard; local records remain authoritative" >&2
    ;;
  *)
    echo "mandate sweep: publish failed; local records remain authoritative" >&2
    ;;
esac
