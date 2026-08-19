#!/usr/bin/env bash
# Merge the base branch into stale builder-authored pull-request heads.

set -uo pipefail
umask 077

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=mandate-lib.sh
source "$SCRIPT_DIR/mandate-lib.sh"

command -v jq >/dev/null 2>&1 || {
  echo "mandate repair: jq is required" >&2
  exit 1
}
command -v git >/dev/null 2>&1 || {
  echo "mandate repair: git is required" >&2
  exit 1
}

usage() {
  echo "usage: repair-prs.sh <builder-lease-owner>" >&2
  exit 2
}

[ "$#" -eq 1 ] || usage
lease_owner="$1"
[ -n "$lease_owner" ] || usage

REPAIR_CAP=3
QUERY_LIMIT=1000
attempted=0
repaired=0
conflicted=0
skipped=0
failed=0
repositories=0
scanned_repositories=0
repository_failures=0
work="$(mktemp -d "${TMPDIR:-/tmp}/ostrom-pr-repair.XXXXXX")" || exit 1

cleanup() {
  rm -rf "$work"
}
trap cleanup EXIT

append_repair_trace() {
  trace_repo="$1"
  trace_number="$2"
  trace_head="$3"
  trace_base="$4"
  trace_outcome="$5"
  trace_head_sha="$6"
  trace_base_sha="$7"
  trace_paths="$8"
  trace_exit_code="$9"
  trace_narration="${10}"

  trace_fact="$(
    jq -cn \
      --arg owner "$lease_owner" \
      --arg repo "$trace_repo" \
      --arg number "$trace_number" \
      --arg head_branch "$trace_head" \
      --arg base_branch "$trace_base" \
      --arg outcome "$trace_outcome" \
      --arg head_sha "$trace_head_sha" \
      --arg base_sha "$trace_base_sha" \
      --arg exit_code "$trace_exit_code" \
      --argjson conflicted_paths "$trace_paths" \
      --argjson cap "$REPAIR_CAP" '
        {
          role: "builder",
          owner: $owner,
          repo: $repo,
          ref: (if $number == "" then null else "#" + $number end),
          action: "merge-base-forward",
          outcome: $outcome,
          head_branch: $head_branch,
          base_branch: $base_branch,
          head_sha: (if $head_sha == "" then null else $head_sha end),
          base_sha: (if $base_sha == "" then null else $base_sha end),
          conflicted_paths: $conflicted_paths,
          cap: $cap
        }
        | if $exit_code == "" then .
          else .exit_code = ($exit_code | tonumber)
          end
      '
  )" || return 1

  OSTROM_HOME="$MANDATE_DATA_DIR" ostrom trace append pr-repair \
    "$trace_fact" "$trace_narration" >/dev/null
}

config="$(mandate_load_config)" || exit
repos_file="$work/repos"
jq -r '.projects[].repo' <<<"$config" >"$repos_file" || exit 1
candidates_file="$work/candidates.jsonl"
: >"$candidates_file"

while IFS= read -r repository; do
  [ -n "$repository" ] || continue
  repositories=$((repositories + 1))
  repo_prs="$work/prs-$(printf '%s' "$repository" | tr '/:' '__').json"
  if ostrom credential builder "$repository" \
    --repositories "$repository" \
    --permissions metadata:read,pull_requests:read -- \
    gh pr list --repo "$repository" --state open --limit "$QUERY_LIMIT" \
      --json number,body,author,mergeable,headRefName,baseRefName,headRefOid,isCrossRepository \
      >"$repo_prs"; then
    :
  else
    query_status=$?
    repository_failures=$((repository_failures + 1))
    echo "mandate repair: failed to enumerate open pull requests for $repository (rc=$query_status)" >&2
    append_repair_trace "$repository" "" "" "" enumeration-failed \
      "" "" '[]' "$query_status" \
      '{"reason":"open pull requests could not be enumerated"}' || exit 1
    continue
  fi
  if ! jq -e 'type == "array"' "$repo_prs" >/dev/null 2>&1; then
    repository_failures=$((repository_failures + 1))
    echo "mandate repair: pull-request listing for $repository was malformed" >&2
    append_repair_trace "$repository" "" "" "" enumeration-malformed \
      "" "" '[]' 1 \
      '{"reason":"open pull-request listing was not an array"}' || exit 1
    continue
  fi
  if [ "$(jq 'length' "$repo_prs")" -eq "$QUERY_LIMIT" ]; then
    repository_failures=$((repository_failures + 1))
    echo "mandate repair: pull-request listing for $repository reached query limit $QUERY_LIMIT; refusing a truncated scan" >&2
    append_repair_trace "$repository" "" "" "" enumeration-truncated \
      "" "" '[]' 6 \
      '{"reason":"open pull-request listing reached the query limit"}' || exit 1
    continue
  fi

  repo_candidates="$work/candidates-$(printf '%s' "$repository" | tr '/:' '__').jsonl"
  if jq -c \
    --arg repo "$repository" '
      .[]
      | select(.mergeable == "CONFLICTING")
      | select(
          (.author.is_bot == true)
          or ((.author.login // "") | endswith("[bot]"))
      )
      | select((.body // "") | test("(^|\\n)Ostrom-Role: builder(\\r?\\n|$)"))
      | select(.isCrossRepository != true)
      | {
          repo: $repo,
          number,
          head: .headRefName,
          base: .baseRefName,
          listed_head_sha: (.headRefOid // "")
        }
    ' "$repo_prs" >"$repo_candidates"; then
    scanned_repositories=$((scanned_repositories + 1))
  else
    repository_failures=$((repository_failures + 1))
    echo "mandate repair: pull-request listing for $repository was malformed" >&2
    append_repair_trace "$repository" "" "" "" enumeration-malformed \
      "" "" '[]' 1 \
      '{"reason":"open pull-request listing could not be filtered"}' || exit 1
    continue
  fi

  while IFS= read -r candidate; do
    [ -n "$candidate" ] || continue
    number="$(jq -r '.number' <<<"$candidate")"
    head_branch="$(jq -r '.head' <<<"$candidate")"
    base_branch="$(jq -r '.base' <<<"$candidate")"
    listed_head_sha="$(jq -r '.listed_head_sha' <<<"$candidate")"
    candidate_checks="$work/checks-$(printf '%s' "$repository" | tr '/:' '__')-$number.json"

    # Deliberately the same four permissions this scan has always requested.
    # #262 never identified which check node was unreadable, and broadening the
    # grant on a guess is tuning until the symptom stops rather than fixing a
    # cause — the failure it was reaching for is now visible and non-fatal as
    # `check-fetch-failed`, which is the mechanism for diagnosing it properly.
    # Widening a role's grant is the principal's call, not this fix's.
    if ostrom credential builder "$repository" \
      --repositories "$repository" \
      --permissions metadata:read,pull_requests:read,checks:read,statuses:read -- \
      gh pr view "$number" --repo "$repository" \
        --json statusCheckRollup >"$candidate_checks"; then
      :
    else
      check_status=$?
      skipped=$((skipped + 1))
      echo "mandate repair: failed to read checks for $repository#$number (rc=$check_status)" >&2
      append_repair_trace "$repository" "$number" "$head_branch" \
        "$base_branch" check-fetch-failed "$listed_head_sha" "" '[]' \
        "$check_status" '{"reason":"candidate check state could not be read"}' || exit 1
      continue
    fi
    if ! jq -e '
      type == "object"
      and (.statusCheckRollup | type == "array")
    ' "$candidate_checks" >/dev/null 2>&1; then
      skipped=$((skipped + 1))
      echo "mandate repair: check state for $repository#$number was malformed" >&2
      append_repair_trace "$repository" "$number" "$head_branch" \
        "$base_branch" check-fetch-malformed "$listed_head_sha" "" '[]' 1 \
        '{"reason":"candidate check state was malformed"}' || exit 1
      continue
    fi
    if jq -e '
      def failure:
        ((.conclusion // .state // "") | ascii_upcase)
        | IN("FAILURE", "ERROR", "CANCELLED", "TIMED_OUT", "ACTION_REQUIRED", "STALE");
      def success:
        ((.conclusion // .state // "") | ascii_upcase)
        | IN("SUCCESS", "NEUTRAL", "SKIPPED");
      .statusCheckRollup as $checks
      | ($checks | length) > 0
      and (any($checks[]?; failure) | not)
      and all($checks[]; success)
    ' "$candidate_checks" >/dev/null; then
      printf '%s\n' "$candidate" >>"$candidates_file"
    fi
  done <"$repo_candidates"
done <"$repos_file"

while IFS= read -r candidate; do
  [ -n "$candidate" ] || continue
  repository="$(jq -r '.repo' <<<"$candidate")"
  number="$(jq -r '.number' <<<"$candidate")"
  head_branch="$(jq -r '.head' <<<"$candidate")"
  base_branch="$(jq -r '.base' <<<"$candidate")"
  listed_head_sha="$(jq -r '.listed_head_sha' <<<"$candidate")"

  if [ "$attempted" -ge "$REPAIR_CAP" ]; then
    skipped=$((skipped + 1))
    append_repair_trace "$repository" "$number" "$head_branch" \
      "$base_branch" skipped-cap "$listed_head_sha" "" '[]' "" \
      '{"reason":"per-pass repair cap reached"}' || exit 1
    continue
  fi
  attempted=$((attempted + 1))

  candidate_work="$work/candidate-$attempted"
  mkdir -p "$candidate_work"
  if git -C "$candidate_work" init --quiet; then
    :
  else
    setup_status=$?
    failed=$((failed + 1))
    append_repair_trace "$repository" "$number" "$head_branch" \
      "$base_branch" local-setup-failed "$listed_head_sha" "" '[]' \
      "$setup_status" \
      '{"reason":"temporary repository initialization failed"}' || exit 1
    continue
  fi
  git -C "$candidate_work" config user.name "Ostrom Builder"
  git -C "$candidate_work" config user.email \
    "ostrom-builder@users.noreply.github.com"

  remote_url="https://github.com/$repository.git"
  if ostrom credential builder "$repository" \
    --repositories "$repository" \
    --permissions metadata:read,contents:read -- \
    git -C "$candidate_work" fetch --no-tags "$remote_url" \
      "refs/heads/$head_branch:refs/remotes/repair/head" \
      "refs/heads/$base_branch:refs/remotes/repair/base"; then
    :
  else
    fetch_status=$?
    failed=$((failed + 1))
    append_repair_trace "$repository" "$number" "$head_branch" \
      "$base_branch" fetch-failed "$listed_head_sha" "" '[]' \
      "$fetch_status" '{"reason":"published branches could not be fetched"}' || exit 1
    continue
  fi

  head_sha="$(git -C "$candidate_work" rev-parse refs/remotes/repair/head)" || {
    resolve_status=$?
    failed=$((failed + 1))
    append_repair_trace "$repository" "$number" "$head_branch" \
      "$base_branch" fetch-failed "$listed_head_sha" "" '[]' \
      "$resolve_status" \
      '{"reason":"fetched head could not be resolved"}' || exit 1
    continue
  }
  base_sha="$(git -C "$candidate_work" rev-parse refs/remotes/repair/base)" || {
    resolve_status=$?
    failed=$((failed + 1))
    append_repair_trace "$repository" "$number" "$head_branch" \
      "$base_branch" fetch-failed "$head_sha" "" '[]' \
      "$resolve_status" \
      '{"reason":"fetched base could not be resolved"}' || exit 1
    continue
  }
  if [ -n "$listed_head_sha" ] && [ "$listed_head_sha" != "$head_sha" ]; then
    failed=$((failed + 1))
    append_repair_trace "$repository" "$number" "$head_branch" \
      "$base_branch" head-moved "$head_sha" "$base_sha" '[]' "" \
      '{"reason":"published head changed after enumeration"}' || exit 1
    continue
  fi

  if git -C "$candidate_work" switch --detach "$head_sha" >/dev/null; then
    :
  else
    setup_status=$?
    failed=$((failed + 1))
    append_repair_trace "$repository" "$number" "$head_branch" \
      "$base_branch" local-setup-failed "$head_sha" "$base_sha" '[]' \
      "$setup_status" \
      '{"reason":"fetched head could not be checked out"}' || exit 1
    continue
  fi
  merge_message="Merge $base_branch into $head_branch

Ostrom-Role: builder"
  if git -C "$candidate_work" merge --no-ff -m "$merge_message" \
    "$base_sha" >/dev/null; then
    :
  else
    merge_status=$?
    conflicted_paths="$(
      git -C "$candidate_work" diff --name-only -z --diff-filter=U |
        jq -Rs 'split("\u0000") | map(select(length > 0))'
    )"
    if [ "$(jq 'length' <<<"$conflicted_paths")" -gt 0 ]; then
      git -C "$candidate_work" merge --abort || true
      conflicted=$((conflicted + 1))
      append_repair_trace "$repository" "$number" "$head_branch" \
        "$base_branch" conflicted "$head_sha" "$base_sha" \
        "$conflicted_paths" "$merge_status" \
        '{"reason":"base-forward merge has content conflicts"}' || exit 1
      continue
    fi
    failed=$((failed + 1))
    append_repair_trace "$repository" "$number" "$head_branch" \
      "$base_branch" merge-failed "$head_sha" "$base_sha" '[]' \
      "$merge_status" '{"reason":"base-forward merge did not complete"}' || exit 1
    continue
  fi

  merge_parents="$(git -C "$candidate_work" rev-list --parents -n 1 HEAD)"
  first_parent="$(awk '{print $2}' <<<"$merge_parents")"
  second_parent="$(awk '{print $3}' <<<"$merge_parents")"
  if [ "$first_parent" != "$head_sha" ] || [ "$second_parent" != "$base_sha" ]; then
    failed=$((failed + 1))
    append_repair_trace "$repository" "$number" "$head_branch" \
      "$base_branch" merge-failed "$head_sha" "$base_sha" '[]' "" \
      '{"reason":"merge commit did not preserve the fetched parents"}' || exit 1
    continue
  fi

  if ostrom credential builder "$repository" \
    --repositories "$repository" \
    --permissions metadata:read,contents:write -- \
    git -C "$candidate_work" push "$remote_url" \
      "HEAD:refs/heads/$head_branch"; then
    repaired=$((repaired + 1))
    append_repair_trace "$repository" "$number" "$head_branch" \
      "$base_branch" repaired "$head_sha" "$base_sha" '[]' 0 '{}' || exit 1
  else
    push_status=$?
    failed=$((failed + 1))
    append_repair_trace "$repository" "$number" "$head_branch" \
      "$base_branch" push-failed "$head_sha" "$base_sha" '[]' \
      "$push_status" '{"reason":"ordinary push was rejected"}' || exit 1
  fi
done <"$candidates_file"

jq -cn \
  --argjson cap "$REPAIR_CAP" \
  --argjson attempted "$attempted" \
  --argjson repaired "$repaired" \
  --argjson conflicted "$conflicted" \
  --argjson skipped "$skipped" \
  --argjson failed "$failed" \
  --argjson repositories "$repositories" \
  --argjson scanned_repositories "$scanned_repositories" \
  --argjson repository_failures "$repository_failures" \
  '{cap: $cap, attempted: $attempted, repaired: $repaired,
    conflicted: $conflicted, skipped: $skipped, failed: $failed,
    repositories: $repositories, scanned_repositories: $scanned_repositories,
    repository_failures: $repository_failures}'

if [ "$repositories" -gt 0 ] && [ "$scanned_repositories" -eq 0 ]; then
  exit 1
fi
