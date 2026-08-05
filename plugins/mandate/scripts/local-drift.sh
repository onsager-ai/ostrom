#!/usr/bin/env bash
# Read-only local Git drift scan. It never fetches, commits, pushes, deletes a
# branch, or changes a working tree.
#
# Classification limit: git cherry is patch-id based, so it catches rebases but
# not squash merges. Squash-merged work can therefore appear unpublished. The
# report exposes both raw commit and not-in-main patch counts; no classification
# by itself is permission to delete a branch.

set -u
umask 077

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=mandate-lib.sh
source "$SCRIPT_DIR/mandate-lib.sh"

local_only=0
case "${1:-}" in
  "") ;;
  --local-only) local_only=1 ;;
  *)
    echo "usage: local-drift.sh [--local-only]" >&2
    exit 2
    ;;
esac

command -v jq >/dev/null 2>&1 || {
  echo "mandate local drift: jq is required" >&2
  exit 1
}
command -v git >/dev/null 2>&1 || {
  echo "mandate local drift: git is required" >&2
  exit 1
}

config="$(mandate_load_config)" || exit
[ "$(jq '.search_roots | length' <<<"$config")" -gt 0 ] || exit 0

work="$(mktemp -d "${TMPDIR:-/tmp}/mandate-local-drift.XXXXXX")" || {
  echo "mandate local drift: could not create temporary workspace" >&2
  exit 1
}
trap 'rm -rf "$work"' EXIT
: >"$work/repos"
: >"$work/rows"

record_row() {
  local first=1 field
  for field in "$@"; do
    if [ "$first" -eq 1 ]; then
      first=0
    else
      printf '\t' >>"$work/rows"
    fi
    printf '%s' "$field" >>"$work/rows"
  done
  printf '\n' >>"$work/rows"
}

add_repository() {
  candidate="$1"
  worktrees="$work/worktrees-candidate"
  git -C "$candidate" worktree list --porcelain >"$worktrees" 2>/dev/null || return 0
  repository="$(sed -n 's/^worktree //p' "$worktrees" | sed -n '1p')"
  [ -n "$repository" ] || return 0
  if [ -d "$repository" ]; then
    repository="$(cd "$repository" 2>/dev/null && pwd -P)" || return 0
  fi
  grep -Fqx "$repository" "$work/repos" 2>/dev/null || printf '%s\n' "$repository" >>"$work/repos"
}

jq -r '.search_roots[]' <<<"$config" >"$work/roots"
while IFS= read -r configured_root; do
  if [ ! -d "$configured_root" ]; then
    record_row "unknown" "root=$configured_root" "reason=search-root-unreadable"
    continue
  fi
  root="$(cd "$configured_root" 2>/dev/null && pwd -P)" || {
    record_row "unknown" "root=$configured_root" "reason=search-root-unreadable"
    continue
  }

  if git -C "$root" rev-parse --git-dir >/dev/null 2>&1; then
    add_repository "$root"
  fi
  find "$root" -name .git -print -prune 2>/dev/null >"$work/git-markers"
  while IFS= read -r marker; do
    add_repository "${marker%/.git}"
  done <"$work/git-markers"
done <"$work/roots"

sort -u "$work/repos" >"$work/repos-sorted"
while IFS= read -r repository; do
  [ -n "$repository" ] || continue
  if ! git -C "$repository" worktree list --porcelain >"$work/worktrees" 2>/dev/null; then
    record_row "unknown" "repository=$repository" "reason=worktree-list-unavailable"
    continue
  fi

  : >"$work/branch-worktrees"
  current_worktree=""
  current_branch="(detached)"
  current_bare=0
  while IFS= read -r worktree_line || [ -n "$worktree_line" ]; do
    case "$worktree_line" in
      worktree\ *)
        current_worktree="${worktree_line#worktree }"
        current_branch="(detached)"
        current_bare=0
        ;;
      branch\ refs/heads/*)
        current_branch="${worktree_line#branch refs/heads/}"
        ;;
      bare)
        current_bare=1
        ;;
      "")
        if [ -n "$current_worktree" ] && [ "$current_bare" -eq 0 ]; then
          if [ "$current_branch" != "(detached)" ]; then
            printf '%s\t%s\n' "$current_branch" "$current_worktree" >>"$work/branch-worktrees"
          fi
          if [ ! -d "$current_worktree" ]; then
            record_row "unknown" "repository=$repository" "worktree=$current_worktree" "reason=worktree-unreadable"
          else
            dirty_count="$(
              git -C "$current_worktree" status --porcelain --untracked-files=normal 2>/dev/null |
                awk 'END { print NR + 0 }'
            )"
            if [ "$dirty_count" -gt 0 ]; then
              record_row "dirty" "repository=$repository" "worktree=$current_worktree" "branch=$current_branch" "changes=$dirty_count"
            fi
          fi
        fi
        current_worktree=""
        current_branch="(detached)"
        current_bare=0
        ;;
    esac
  done <"$work/worktrees"
  # Porcelain currently ends in a blank line, but flush a future/older format
  # that does not without duplicating the normal case.
  if [ -n "$current_worktree" ] && [ "$current_bare" -eq 0 ]; then
    if [ "$current_branch" != "(detached)" ]; then
      printf '%s\t%s\n' "$current_branch" "$current_worktree" >>"$work/branch-worktrees"
    fi
    if [ ! -d "$current_worktree" ]; then
      record_row "unknown" "repository=$repository" "worktree=$current_worktree" "reason=worktree-unreadable"
    else
      dirty_count="$(
        git -C "$current_worktree" status --porcelain --untracked-files=normal 2>/dev/null |
          awk 'END { print NR + 0 }'
      )"
      if [ "$dirty_count" -gt 0 ]; then
        record_row "dirty" "repository=$repository" "worktree=$current_worktree" "branch=$current_branch" "changes=$dirty_count"
      fi
    fi
  fi

  if ! git -C "$repository" rev-parse --verify --quiet refs/remotes/origin/main >/dev/null; then
    record_row "unknown" "repository=$repository" "reason=origin-main-unavailable"
    continue
  fi

  git -C "$repository" for-each-ref --format='%(refname:short)' refs/heads \
    >"$work/branches" 2>/dev/null || {
      record_row "unknown" "repository=$repository" "reason=local-branches-unavailable"
      continue
    }
  while IFS= read -r branch; do
    [ -n "$branch" ] || continue
    raw_commits="$(git -C "$repository" rev-list --count "origin/main..refs/heads/$branch" 2>/dev/null)" || {
      record_row "unknown" "repository=$repository" "branch=$branch" "reason=commit-count-unavailable"
      continue
    }
    [ "$raw_commits" -gt 0 ] || continue

    if ! git -C "$repository" cherry origin/main "refs/heads/$branch" >"$work/cherry" 2>/dev/null; then
      record_row "unknown" "repository=$repository" "branch=$branch" "raw_commits=$raw_commits" "reason=patch-classification-unavailable"
      continue
    fi
    patches_not_in_main="$(awk '$1 == "+" { count++ } END { print count + 0 }' "$work/cherry")"
    branch_worktree="$(awk -F '\t' -v branch="$branch" '$1 == branch { print substr($0, length($1) + 2); exit }' "$work/branch-worktrees")"
    [ -n "$branch_worktree" ] || branch_worktree="-"

    if [ "$patches_not_in_main" -eq 0 ]; then
      record_row "landed" "repository=$repository" "worktree=$branch_worktree" "branch=$branch" "raw_commits=$raw_commits" "patches_not_in_main=0" "review=cleanup-candidate-not-delete-proof"
      continue
    fi

    upstream="$(git -C "$repository" for-each-ref --format='%(upstream:short)' "refs/heads/$branch" 2>/dev/null)"
    if [ -z "$upstream" ]; then
      record_row "unpublished" "repository=$repository" "worktree=$branch_worktree" "branch=$branch" "raw_commits=$raw_commits" "patches_not_in_main=$patches_not_in_main" "publication=unpushed-no-upstream"
      continue
    fi
    ahead_of_upstream="$(git -C "$repository" rev-list --count "$upstream..refs/heads/$branch" 2>/dev/null)" || {
      record_row "unpublished" "repository=$repository" "worktree=$branch_worktree" "branch=$branch" "raw_commits=$raw_commits" "patches_not_in_main=$patches_not_in_main" "publication=upstream-status-unknown"
      continue
    }
    if [ "$ahead_of_upstream" -gt 0 ]; then
      record_row "unpublished" "repository=$repository" "worktree=$branch_worktree" "branch=$branch" "raw_commits=$raw_commits" "patches_not_in_main=$patches_not_in_main" "publication=unpushed-ahead-by-$ahead_of_upstream"
      continue
    fi

    # SessionStart is intentionally network-free in this repository. Its local
    # mode suppresses fully pushed branches; an interactive run performs the PR
    # lookup and can distinguish an open/merged PR from the next-stage failure.
    [ "$local_only" -eq 0 ] || continue
    gh_directory="$repository"
    [ "$branch_worktree" = "-" ] || gh_directory="$branch_worktree"
    if ! command -v gh >/dev/null 2>&1 ||
      ! git -C "$gh_directory" rev-parse --git-dir >/dev/null 2>&1 ||
      ! (cd "$gh_directory" && gh pr list --head "$branch" --state all --limit 100 \
        --json state,mergedAt) >"$work/prs" 2>/dev/null; then
      record_row "unpublished" "repository=$repository" "worktree=$branch_worktree" "branch=$branch" "raw_commits=$raw_commits" "patches_not_in_main=$patches_not_in_main" "publication=pr-status-unknown"
      continue
    fi
    if ! jq -e 'type == "array"' "$work/prs" >/dev/null 2>&1; then
      record_row "unpublished" "repository=$repository" "worktree=$branch_worktree" "branch=$branch" "raw_commits=$raw_commits" "patches_not_in_main=$patches_not_in_main" "publication=pr-status-unknown"
      continue
    fi
    if jq -e 'any(.[]; .state == "OPEN" or .state == "MERGED" or .mergedAt != null)' \
      "$work/prs" >/dev/null; then
      continue
    fi
    record_row "unpublished" "repository=$repository" "worktree=$branch_worktree" "branch=$branch" "raw_commits=$raw_commits" "patches_not_in_main=$patches_not_in_main" "publication=pushed-no-open-pr-or-merge"
  done <"$work/branches"
done <"$work/repos-sorted"

[ -s "$work/rows" ] || exit 0
echo "LOCAL DRIFT"
echo "LIMIT: git cherry is patch-id based: it catches rebases but not squash merges; squash-merged work can appear unpublished. Counts are raw_commits / patches_not_in_main; review before deleting."
cat "$work/rows"
