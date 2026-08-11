#!/usr/bin/env bash
# Read-only GitHub merge gate. The only write is the machine-local verdict log.
#
# Verdict exit codes:
#   0  pass         every condition is met
#   1  fail         at least one condition is definitively unmet
#   2  inconclusive no condition failed, but at least one could not be evaluated
#  64  usage error  the input is not exactly owner/repo#number

set -uo pipefail
umask 077

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=mandate-lib.sh
source "$SCRIPT_DIR/mandate-lib.sh"

EXIT_PASS=0
EXIT_FAIL=1
EXIT_INCONCLUSIVE=2
EXIT_USAGE=64

usage() {
  echo "usage: gate.sh <owner/repo#number>" >&2
  exit "$EXIT_USAGE"
}

[ "$#" -eq 1 ] || usage
target="$1"
if [[ ! "$target" =~ ^[^/[:space:]#]+/[^/[:space:]#]+#[1-9][0-9]*$ ]]; then
  usage
fi
repo="${target%#*}"
number="${target##*#}"
owner="${repo%%/*}"
repo_name="${repo#*/}"

if ! command -v jq >/dev/null 2>&1; then
  echo "verdict: inconclusive pr=$target head_sha=unknown already_judged=false"
  echo "mandate gate: jq is required" >&2
  exit "$EXIT_INCONCLUSIVE"
fi
if ! command -v gh >/dev/null 2>&1; then
  echo "verdict: inconclusive pr=$target head_sha=unknown already_judged=false"
  echo "mandate gate: gh is required" >&2
  exit "$EXIT_INCONCLUSIVE"
fi

work="$(mktemp -d "${TMPDIR:-/tmp}/mandate-gate.XXXXXX")" || {
  echo "verdict: inconclusive pr=$target head_sha=unknown already_judged=false"
  echo "mandate gate: could not create temporary workspace" >&2
  exit "$EXIT_INCONCLUSIVE"
}
trap 'rm -rf "$work"' EXIT

error_text() {
  error_file="$1"
  if [ -s "$error_file" ]; then
    LC_ALL=C tr '\n' ' ' <"$error_file" | cut -c 1-500
  else
    printf '%s' "no error detail returned"
  fi
}

write_condition() {
  condition_file="$1"
  condition_name="$2"
  condition_result="$3"
  condition_tiers="$4"
  condition_detail="$5"
  jq -cn \
    --arg name "$condition_name" \
    --arg result "$condition_result" \
    --argjson tier "$condition_tiers" \
    --argjson detail "$condition_detail" \
    '{name: $name, result: $result, tier: $tier, detail: $detail}' \
    >"$condition_file"
}

config_ready=0
config_error=""
if ! mandate_gate_is_configured; then
  config_error="no gate.yaml found at $MANDATE_GATE_USER_CONFIG or $MANDATE_GATE_REPO_CONFIG"
elif mandate_load_gate_config >"$work/config.json" 2>"$work/config-error"; then
  if jq -e --arg repo "$repo" '
    [.projects[] | select(.repo == $repo)] | length == 1
  ' "$work/config.json" >/dev/null; then
    jq -c --arg repo "$repo" 'first(.projects[] | select(.repo == $repo))' \
      "$work/config.json" >"$work/project.json"
    config_ready=1
  else
    config_error="gate.yaml has no project entry for $repo"
  fi
else
  config_error="$(error_text "$work/config-error")"
fi

metadata_ready=0
head_sha=""
if gh pr view "$number" --repo "$repo" \
  --json number,title,author,headRefOid,labels,statusCheckRollup,closingIssuesReferences,mergeable,isDraft \
  >"$work/pr.json" 2>"$work/pr-error"; then
  head_sha="$(jq -r '
    if (.headRefOid | type) == "string" then .headRefOid else empty end
  ' "$work/pr.json" 2>/dev/null)"
  if jq -e --argjson number "$number" '
    type == "object"
    and .number == $number
    and (.title | type == "string")
    and (.author.login | type == "string" and length > 0)
    and (.headRefOid | type == "string" and length > 0)
    and (.labels | type == "array")
    and (.statusCheckRollup | type == "array")
    and (.closingIssuesReferences | type == "array")
    and (.mergeable | type == "string")
    and (.isDraft | type == "boolean")
  ' "$work/pr.json" >/dev/null 2>&1; then
    metadata_ready=1
  else
    printf '%s\n' "pull request metadata was incomplete" >"$work/pr-error"
  fi
fi
metadata_error="$(error_text "$work/pr-error")"
if [ "$metadata_ready" -eq 1 ]; then
  cp "$work/pr.json" "$work/pr-safe.json"
else
  printf '%s\n' '{}' >"$work/pr-safe.json"
fi

diff_ready=0
if gh pr diff "$number" --repo "$repo" --name-only \
  >"$work/paths" 2>"$work/diff-error"; then
  jq -Rsc 'split("\n") | map(select(length > 0))' "$work/paths" \
    >"$work/paths.json"
  diff_ready=1
else
  printf '%s\n' '[]' >"$work/paths.json"
fi
diff_error="$(error_text "$work/diff-error")"

threads_ready=0
threads_error=""
thread_author=""
: >"$work/threads.jsonl"
review_query='query($owner:String!, $repo:String!, $number:Int!, $cursor:String) {
  repository(owner:$owner, name:$repo) {
    pullRequest(number:$number) {
      author { login }
      reviewThreads(first:100, after:$cursor) {
        nodes {
          id
          isResolved
          resolvedBy { login }
          comments(last:1) { nodes { author { login } } }
        }
        pageInfo { hasNextPage endCursor }
      }
    }
  }
}'
cursor=""
page=0
while [ "$page" -lt 100 ]; do
  page=$((page + 1))
  if [ -n "$cursor" ]; then
    gh api graphql \
      -f query="$review_query" \
      -f owner="$owner" \
      -f repo="$repo_name" \
      -F number="$number" \
      -f cursor="$cursor" \
      >"$work/thread-page.json" 2>"$work/thread-error"
  else
    gh api graphql \
      -f query="$review_query" \
      -f owner="$owner" \
      -f repo="$repo_name" \
      -F number="$number" \
      >"$work/thread-page.json" 2>"$work/thread-error"
  fi
  graph_status=$?
  if [ "$graph_status" -ne 0 ]; then
    threads_error="$(error_text "$work/thread-error")"
    break
  fi
  if ! jq -e '
    ((.errors // []) | length) == 0
    and (.data.repository.pullRequest | type == "object")
    and (.data.repository.pullRequest.author.login | type == "string" and length > 0)
    and (.data.repository.pullRequest.reviewThreads.nodes | type == "array")
    and (.data.repository.pullRequest.reviewThreads.pageInfo.hasNextPage | type == "boolean")
    and all(.data.repository.pullRequest.reviewThreads.nodes[];
      (.id | type == "string" and length > 0)
      and (.isResolved | type == "boolean")
      and (
        .resolvedBy == null
        or (
          (.resolvedBy | type == "object")
          and (.resolvedBy.login | type == "string" and length > 0)
        )
      )
      and (.comments.nodes | type == "array")
      and all(.comments.nodes[];
        (.author == null)
        or ((.author | type == "object") and (.author.login | type == "string" and length > 0))
      )
    )
  ' "$work/thread-page.json" >/dev/null 2>&1; then
    threads_error="review-thread query returned missing data or an API error"
    break
  fi
  page_author="$(jq -r '.data.repository.pullRequest.author.login // empty' \
    "$work/thread-page.json")"
  if [ -z "$thread_author" ]; then
    thread_author="$page_author"
  elif [ "$page_author" != "$thread_author" ]; then
    threads_error="pull request author changed during review-thread pagination"
    break
  fi
  jq -c '.data.repository.pullRequest.reviewThreads.nodes[]' \
    "$work/thread-page.json" >>"$work/threads.jsonl"
  has_next="$(jq -r '.data.repository.pullRequest.reviewThreads.pageInfo.hasNextPage' \
    "$work/thread-page.json")"
  if [ "$has_next" = "false" ]; then
    threads_ready=1
    break
  fi
  next_cursor="$(jq -r '.data.repository.pullRequest.reviewThreads.pageInfo.endCursor // empty' \
    "$work/thread-page.json")"
  if [ -z "$next_cursor" ] || [ "$next_cursor" = "$cursor" ]; then
    threads_error="review-thread pagination did not return a new cursor"
    break
  fi
  cursor="$next_cursor"
done
if [ "$threads_ready" -eq 0 ] && [ -z "$threads_error" ]; then
  threads_error="review-thread query exceeded 100 pages"
fi

if [ "$config_ready" -eq 0 ]; then
  detail="$(jq -cn --arg reason "$config_error" '{reason: $reason}')"
  write_condition "$work/checks-condition.json" required_checks inconclusive '[]' "$detail"
  write_condition "$work/threads-condition.json" review_threads inconclusive '[]' "$detail"
  write_condition "$work/bounce-condition.json" bounce_selectors inconclusive '[]' "$detail"
  write_condition "$work/reserved-condition.json" reserved_refs inconclusive '[]' "$detail"
  detail="$(jq -cn --arg reason "$config_error" '{mergeable: null, reason: $reason}')"
  write_condition "$work/mergeable-condition.json" mergeable inconclusive '[]' "$detail"
  detail="$(jq -cn --arg reason "$config_error" '{isDraft: null, reason: $reason}')"
  write_condition "$work/draft-condition.json" draft inconclusive '[]' "$detail"
else
  if [ "$metadata_ready" -eq 0 ]; then
    detail="$(jq -cn --arg reason "$metadata_error" '{mergeable: null, reason: $reason}')"
    write_condition "$work/mergeable-condition.json" mergeable inconclusive '["content-derived"]' "$detail"
    detail="$(jq -cn --arg reason "$metadata_error" '{isDraft: null, reason: $reason}')"
    write_condition "$work/draft-condition.json" draft inconclusive '["content-derived"]' "$detail"
  else
    jq '
      {
        name: "mergeable",
        result: (
          if .mergeable == "MERGEABLE" then "pass"
          elif .mergeable == "CONFLICTING" then "fail"
          else "inconclusive"
          end
        ),
        tier: ["content-derived"],
        detail: {mergeable: .mergeable}
      }
    ' "$work/pr-safe.json" >"$work/mergeable-condition.json"
    jq '
      {
        name: "draft",
        result: (if .isDraft then "fail" else "pass" end),
        tier: ["content-derived"],
        detail: {isDraft: .isDraft}
      }
    ' "$work/pr-safe.json" >"$work/draft-condition.json"
  fi

  if [ "$metadata_ready" -eq 0 ]; then
    detail="$(jq -cn --arg reason "$metadata_error" '{reason: $reason}')"
    write_condition "$work/checks-condition.json" required_checks inconclusive '["content-derived"]' "$detail"
  else
    jq -cn \
      --slurpfile project "$work/project.json" \
      --slurpfile pr "$work/pr-safe.json" '
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
        def check_name: (.name // .context // "");
        def check_state: ((.conclusion // .state // .status // "") | ascii_upcase);
        def green: check_state | IN("SUCCESS", "NEUTRAL", "SKIPPED");
        def known_not_green:
          check_state | IN(
            "FAILURE", "ERROR", "CANCELLED", "TIMED_OUT", "ACTION_REQUIRED",
            "STALE", "PENDING", "EXPECTED", "QUEUED", "IN_PROGRESS",
            "WAITING", "REQUESTED"
          );
        ($project[0].required_checks) as $selectors
        | ($pr[0].statusCheckRollup) as $checks
        | ([
            $selectors[]
            | . as $selector
            | ([$checks[] | select(glob_match(check_name; $selector; false))]) as $matches
            | {
                selector: $selector,
                result: (
                  if ($matches | length) == 0 then "fail"
                  elif any($matches[]; green | not) and any($matches[]; known_not_green)
                  then "fail"
                  elif any($matches[]; (green or known_not_green) | not)
                  then "inconclusive"
                  else "pass"
                  end
                ),
                matches: [$matches[] | {name: check_name, state: check_state}]
              }
          ]) as $selected
        | {
            name: "required_checks",
            result: (
              if any($selected[]; .result == "fail") then "fail"
              elif any($selected[]; .result == "inconclusive") then "inconclusive"
              else "pass"
              end
            ),
            tier: (if ($selectors | length) > 0 then ["content-derived"] else [] end),
            detail: {selectors: $selected}
          }
      ' >"$work/checks-condition.json"
  fi

  if [ "$threads_ready" -eq 0 ]; then
    detail="$(jq -cn --arg reason "$threads_error" '{reason: $reason}')"
    write_condition "$work/threads-condition.json" review_threads inconclusive '["content-derived"]' "$detail"
  else
    if [ -s "$work/threads.jsonl" ]; then
      # answered/unanswered split open threads only (isResolved == false) by
      # who spoke last: the PR author replying after the reviewer's most
      # recent comment makes a thread "answered". This is informational only
      # — it must NEVER change $result. An answered thread is still open work
      # for the gate: the author cannot clear a thread by replying to it any
      # more than by resolving it outright, because a reply is exactly as
      # much an unverified self-assertion as a resolution is. Do not fold
      # "answered" into the pass/fail logic, and do not let an author's own
      # reply count toward resolved_by_pr_author below — that field tracks a
      # different, already-forbidden act (self-resolving the thread), not
      # replying to it, and the two must stay distinguishable.
      jq -s --arg author "$thread_author" '
        ([.[] | select(.isResolved == false)]) as $open
        | ($open | length) as $unresolved
        | ([$open[] | select(
              ((.comments.nodes[-1].author.login // "") | length) > 0
              and ((.comments.nodes[-1].author.login // "") | ascii_downcase)
                == ($author | ascii_downcase)
            )] | length) as $answered
        | ($unresolved - $answered) as $unanswered
        | ([.[] | select(
              .isResolved == true
              and ((.resolvedBy.login // "") | ascii_downcase)
                == ($author | ascii_downcase)
            )] | length) as $by_author
        | ([.[] | select(
              .isResolved == true
              and ((.resolvedBy.login // "") | length) == 0
            )] | length) as $missing_resolver
        | {
            name: "review_threads",
            result: (
              if $unresolved > 0 or $by_author > 0 then "fail"
              elif $missing_resolver > 0 or ($author | length) == 0 then "inconclusive"
              else "pass"
              end
            ),
            tier: ["content-derived"],
            detail: {
              unresolved: $unresolved,
              answered: $answered,
              unanswered: $unanswered,
              resolved_by_pr_author: $by_author,
              resolved_with_missing_resolver: $missing_resolver
            }
          }
      ' "$work/threads.jsonl" >"$work/threads-condition.json"
    else
      detail='{"unresolved":0,"answered":0,"unanswered":0,"resolved_by_pr_author":0,"resolved_with_missing_resolver":0}'
      write_condition "$work/threads-condition.json" review_threads pass '["content-derived"]' "$detail"
    fi
  fi

  jq -cn \
    --argjson metadata_ready "$metadata_ready" \
    --argjson diff_ready "$diff_ready" \
    --arg metadata_error "$metadata_error" \
    --arg diff_error "$diff_error" \
    --slurpfile config "$work/config.json" \
    --slurpfile project "$work/project.json" \
    --slurpfile pr "$work/pr-safe.json" \
    --slurpfile paths "$work/paths.json" '
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
      def tier($selector):
        ($selector | capture("^(?<prefix>[^:]+):").prefix) as $prefix
        | if $prefix == "path" or $prefix == "ref"
          then "content-derived"
          else "author-written"
          end;
      (($pr[0] // {}) as $raw
        | {
            type: "pr",
            title: ($raw.title // ""),
            labels: [($raw.labels // [])[]? | .name // empty],
            files: ($paths[0] // []),
            refs: (
              [($raw.number // empty)]
              + [($raw.closingIssuesReferences // [])[]? | .number // empty]
              | unique
            )
          }) as $item
      | (($config[0].bounce_all // []) + ($project[0].bounce // [])) as $selectors
      | ([
          $selectors[]
          | . as $selector
          | ($selector | capture("^(?<prefix>[^:]+):").prefix) as $prefix
          | (if $prefix == "path" then $diff_ready == 1 else $metadata_ready == 1 end) as $observable
          | {
              selector: $selector,
              tier: tier($selector),
              observable: $observable,
              matched: (if $observable then selector_match($item; $selector) else false end),
              error: (
                if $observable then null
                elif $prefix == "path" then $diff_error
                else $metadata_error
                end
              )
            }
        ]) as $evaluated
      | ([$evaluated[] | select(.matched) | {selector, tier}]) as $matches
      | ([$evaluated[] | select(.observable | not) | {selector, tier, error}]) as $unknown
      | {
          name: "bounce_selectors",
          result: (
            if ($matches | length) > 0 then "fail"
            elif ($unknown | length) > 0 then "inconclusive"
            else "pass"
            end
          ),
          tier: (
            if ($matches | length) > 0
            then [$matches[].tier] | unique
            elif ($unknown | length) > 0
            then [$unknown[].tier] | unique
            else []
            end
          ),
          detail: {matches: $matches, unobservable: $unknown}
        }
    ' >"$work/bounce-condition.json"

  if [ "$metadata_ready" -eq 0 ]; then
    detail="$(jq -cn --arg reason "$metadata_error" '{reason: $reason}')"
    write_condition "$work/reserved-condition.json" reserved_refs inconclusive '["content-derived"]' "$detail"
  else
    jq -cn \
      --slurpfile project "$work/project.json" \
      --slurpfile pr "$work/pr-safe.json" '
        (([($pr[0].number)]
          + [($pr[0].closingIssuesReferences // [])[]? | .number // empty])
          | unique) as $refs
        | ([$project[0].reserved[] | select(. as $number | any($refs[]; . == $number))]) as $matches
        | {
            name: "reserved_refs",
            result: (if ($matches | length) > 0 then "fail" else "pass" end),
            tier: (if ($matches | length) > 0 then ["content-derived"] else [] end),
            detail: {matches: [$matches[] | "ref:#" + tostring]}
          }
      ' >"$work/reserved-condition.json"
  fi
fi

jq -s '.' \
  "$work/mergeable-condition.json" \
  "$work/draft-condition.json" \
  "$work/checks-condition.json" \
  "$work/threads-condition.json" \
  "$work/bounce-condition.json" \
  "$work/reserved-condition.json" \
  >"$work/evaluated-conditions.json"

# Exception records use the same append-only JSONL tuple lookup as the
# already_judged delivery guard below. The extra repo and condition fields
# keep a grant scoped to one condition of one PR artifact.
printf '%s\n' '[]' >"$work/matching-exceptions.json"
if [ -n "$head_sha" ] && [ -s "$MANDATE_EXCEPTIONS_LOG" ]; then
  jq -s \
    --arg repo "$repo" \
    --argjson pr "$number" \
    --arg head_sha "$head_sha" '
      [.[] | select(
        .repo == $repo
        and .pr == $pr
        and .head_sha == $head_sha
      )]
    ' "$MANDATE_EXCEPTIONS_LOG" >"$work/matching-exceptions.json" 2>/dev/null || {
    # Fail closed, but never silently. A corrupted log means valid grants stop
    # applying, and the resulting `fail` is indistinguishable from a PR that
    # was never excused — the principal would re-grant an exception that is
    # already there and watch it do nothing.
    echo "mandate gate: could not read $MANDATE_EXCEPTIONS_LOG; ignoring all exceptions" >&2
    printf '%s\n' '[]' >"$work/matching-exceptions.json"
  }
fi

jq --slurpfile exceptions "$work/matching-exceptions.json" '
  map(
    . as $condition
    | if (.result == "fail" or .result == "inconclusive")
      then ([
        $exceptions[0][]
        | select(.condition == $condition.name)
      ] | last) as $exception
      | if $exception == null
        then .
        else .result = "excused" | .exception_reason = $exception.reason
        end
      else .
      end
  )
' "$work/evaluated-conditions.json" >"$work/conditions.json"

verdict="$(jq -r '
  if any(.[]; .result == "fail") then "fail"
  elif any(.[]; .result == "inconclusive") then "inconclusive"
  else "pass"
  end
' "$work/conditions.json")"

already_judged=false
if [ -n "$head_sha" ] && [ -s "$MANDATE_GATE_LOG" ] &&
  jq -s -e --arg pr "$target" --arg head_sha "$head_sha" '
    any(.[]; .pr == $pr and .head_sha == $head_sha)
  ' "$MANDATE_GATE_LOG" >/dev/null 2>&1; then
  already_judged=true
fi

record="$(
  jq -cn \
    --arg ts "${MANDATE_GATE_TIME:-$(date -u +%Y-%m-%dT%H:%M:%SZ)}" \
    --arg pr "$target" \
    --arg head_sha "$head_sha" \
    --arg verdict "$verdict" \
    --argjson already_judged "$already_judged" \
    --slurpfile conditions "$work/conditions.json" '
      {
        ts: $ts,
        pr: $pr,
        head_sha: (if ($head_sha | length) > 0 then $head_sha else null end),
        verdict: $verdict,
        already_judged: $already_judged,
        conditions: $conditions[0]
      }
    '
)"
if ! mkdir -p "$MANDATE_DATA_DIR" || ! printf '%s\n' "$record" >>"$MANDATE_GATE_LOG"; then
  echo "verdict: inconclusive pr=$target head_sha=${head_sha:-unknown} already_judged=$already_judged"
  echo "mandate gate: could not append $MANDATE_GATE_LOG" >&2
  exit "$EXIT_INCONCLUSIVE"
fi

jq -r '
  "verdict: \(.verdict) pr=\(.pr) head_sha=\(.head_sha // "unknown") already_judged=\(.already_judged)",
  (.conditions[]
    | "condition \(.name): \(.result) tier="
      + (if (.tier | length) == 0 then "none" else (.tier | join(",")) end)
      + (if has("exception_reason") then " exception_reason=" + (.exception_reason | tojson) else "" end)
      + " detail=" + (.detail | tojson)
  )
' <<<"$record"

case "$verdict" in
  pass) exit "$EXIT_PASS" ;;
  fail) exit "$EXIT_FAIL" ;;
  inconclusive) exit "$EXIT_INCONCLUSIVE" ;;
esac
