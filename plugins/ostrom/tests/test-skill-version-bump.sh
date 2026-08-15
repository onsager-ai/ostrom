#!/usr/bin/env bash

set -Eeuo pipefail

repo_root="$(git rev-parse --show-toplevel)"
guard="$repo_root/plugins/ostrom/tests/check-skill-version-bump.sh"
test_root="$(mktemp -d)"
trap 'rm -rf -- "$test_root"' EXIT

new_repo() {
  local name="$1"
  local repo="$test_root/$name"
  mkdir -p \
    "$repo/plugins/alpha/skills/work" \
    "$repo/plugins/alpha/.claude-plugin" \
    "$repo/plugins/beta/.claude-plugin" \
    "$repo/docs"
  printf '%s\n' '# alpha protocol' >"$repo/plugins/alpha/skills/work/SKILL.md"
  printf '%s\n' '{"name":"alpha","version":"1.0.0"}' \
    >"$repo/plugins/alpha/.claude-plugin/plugin.json"
  printf '%s\n' '{"name":"beta","version":"2.0.0"}' \
    >"$repo/plugins/beta/.claude-plugin/plugin.json"
  printf '%s\n' '# fixture' >"$repo/README.md"
  (
    cd "$repo"
    git init --quiet --initial-branch=main
    git config user.name 'Ostrom Test'
    git config user.email 'ostrom@example.test'
    git add .
    git commit --quiet -m base
    git branch base
  )
  printf '%s\n' "$repo"
}

commit_all() {
  local repo="$1"
  (
    cd "$repo"
    git add .
    git commit --quiet -m change
  )
}

repo="$(new_repo unbumped)"
printf '%s\n' '# changed protocol' >>"$repo/plugins/alpha/skills/work/SKILL.md"
commit_all "$repo"
set +e
unbumped_output="$(cd "$repo" && bash "$guard" base HEAD 2>&1)"
unbumped_status=$?
set -e
[[ "$unbumped_status" -eq 1 ]]
grep -Fq "plugin 'alpha'" <<<"$unbumped_output"
grep -Fq "plugins/alpha/skills/work/SKILL.md" <<<"$unbumped_output"

repo="$(new_repo correctly-bumped)"
printf '%s\n' '# changed protocol' >>"$repo/plugins/alpha/skills/work/SKILL.md"
printf '%s\n' '{"name":"alpha","version":"1.0.1"}' \
  >"$repo/plugins/alpha/.claude-plugin/plugin.json"
commit_all "$repo"
(cd "$repo" && bash "$guard" base HEAD)

repo="$(new_repo docs-only)"
printf '%s\n' 'Documentation only.' >>"$repo/README.md"
printf '%s\n' 'More documentation.' >"$repo/docs/guide.md"
commit_all "$repo"
(cd "$repo" && bash "$guard" base HEAD)

repo="$(new_repo wrong-plugin)"
printf '%s\n' '# changed protocol' >>"$repo/plugins/alpha/skills/work/SKILL.md"
printf '%s\n' '{"name":"beta","version":"2.0.1"}' \
  >"$repo/plugins/beta/.claude-plugin/plugin.json"
commit_all "$repo"
set +e
wrong_plugin_output="$(cd "$repo" && bash "$guard" base HEAD 2>&1)"
wrong_plugin_status=$?
set -e
[[ "$wrong_plugin_status" -eq 1 ]]
grep -Fq "plugin 'alpha'" <<<"$wrong_plugin_output"
grep -Fq "plugins/alpha/skills/work/SKILL.md" <<<"$wrong_plugin_output"

echo "skill version bump tests: ok"
