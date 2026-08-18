#!/usr/bin/env bash

set -Eeuo pipefail

repo_root="$(git rev-parse --show-toplevel)"
guard="$repo_root/plugins/ostrom/tests/check-shell-freeze.sh"
test_root="$(mktemp -d)"
trap 'rm -rf -- "$test_root"' EXIT

new_repo() {
  local name="$1"
  local repo="$test_root/$name"
  mkdir -p "$repo/plugins/ostrom/scripts" "$repo/plugins/ostrom/tests"
  printf '%s\n' '#!/usr/bin/env bash' 'printf alpha' \
    >"$repo/plugins/ostrom/scripts/grow.sh"
  printf '%s\n' '#!/usr/bin/env bash' 'printf alpha' 'printf beta' \
    >"$repo/plugins/ostrom/scripts/shrink.sh"
  printf '%s\n' '#!/usr/bin/env bash' 'printf alpha' \
    >"$repo/plugins/ostrom/scripts/delete.sh"
  printf '%s\n' '#!/usr/bin/env bash' 'printf alpha' 'printf beta' \
    >"$repo/plugins/ostrom/scripts/mixed-shrink.sh"
  printf '%s\n' '#!/usr/bin/env bash' 'printf alpha' \
    >"$repo/plugins/ostrom/scripts/mixed-grow.sh"
  # The exempt script needs a real baseline so its case is genuine growth
  # rather than a new file.
  printf '%s\n' '#!/usr/bin/env bash' 'printf alpha' \
    >"$repo/plugins/ostrom/scripts/run-node.sh"
  printf '%s\n' '# fixture' >"$repo/README.md"
  printf '%s\n' '# fixture test' >"$repo/plugins/ostrom/tests/fixture.sh"
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

repo="$(new_repo growth)"
printf '%s\n' 'printf beta' 'printf gamma' \
  >>"$repo/plugins/ostrom/scripts/grow.sh"
commit_all "$repo"
set +e
growth_output="$(cd "$repo" && bash "$guard" base HEAD 2>&1)"
growth_status=$?
set -e
[[ "$growth_status" -eq 1 ]]
grep -Fq 'plugins/ostrom/scripts/grow.sh' <<<"$growth_output"
grep -Fq 'grew by 2 lines (2 -> 4)' <<<"$growth_output"
grep -Fq 'implement this in Rust' <<<"$growth_output"
grep -Fq 'bash-bugfix' <<<"$growth_output"

set +e
labelled_output="$({
  cd "$repo"
  PULL_REQUEST_LABELS=$'alpha\nbash-bugfix\nbeta' bash "$guard" base HEAD
} 2>&1)"
labelled_status=$?
set -e
[[ "$labelled_status" -eq 0 ]]
grep -Fq 'plugins/ostrom/scripts/grow.sh' <<<"$labelled_output"
grep -Fq 'grew by 2 lines (2 -> 4)' <<<"$labelled_output"
grep -Fq 'permitted by the bash-bugfix label' <<<"$labelled_output"

repo="$(new_repo shrink)"
printf '%s\n' '#!/usr/bin/env bash' >"$repo/plugins/ostrom/scripts/shrink.sh"
commit_all "$repo"
(cd "$repo" && bash "$guard" base HEAD)

repo="$(new_repo unchanged)"
(cd "$repo" && bash "$guard" base HEAD)

repo="$(new_repo added)"
printf '%s\n' '#!/usr/bin/env bash' 'printf alpha' 'printf beta' \
  >"$repo/plugins/ostrom/scripts/added.sh"
commit_all "$repo"
set +e
added_output="$(cd "$repo" && bash "$guard" base HEAD 2>&1)"
added_status=$?
set -e
[[ "$added_status" -eq 1 ]]
grep -Fq 'plugins/ostrom/scripts/added.sh' <<<"$added_output"
grep -Fq 'grew by 3 lines (0 -> 3)' <<<"$added_output"

repo="$(new_repo deleted)"
rm "$repo/plugins/ostrom/scripts/delete.sh"
commit_all "$repo"
(cd "$repo" && bash "$guard" base HEAD)

repo="$(new_repo outside)"
printf '%s\n' 'Documentation only.' >>"$repo/README.md"
printf '%s\n' '# changed fixture test' \
  >>"$repo/plugins/ostrom/tests/fixture.sh"
commit_all "$repo"
(cd "$repo" && bash "$guard" base HEAD)

repo="$(new_repo mixed)"
printf '%s\n' '#!/usr/bin/env bash' \
  >"$repo/plugins/ostrom/scripts/mixed-shrink.sh"
printf '%s\n' 'printf beta' 'printf gamma' \
  >>"$repo/plugins/ostrom/scripts/mixed-grow.sh"
commit_all "$repo"
set +e
mixed_output="$(cd "$repo" && bash "$guard" base HEAD 2>&1)"
mixed_status=$?
set -e
[[ "$mixed_status" -eq 1 ]]
grep -Fq 'plugins/ostrom/scripts/mixed-grow.sh' <<<"$mixed_output"
grep -Fq 'grew by 2 lines (2 -> 4)' <<<"$mixed_output"
[[ "$mixed_output" != *'plugins/ostrom/scripts/mixed-shrink.sh'* ]]

# An exempt script may grow without a label. It is exempt from the retirement,
# so freezing it would only guarantee it rots while nobody is allowed to delete
# it either.
repo="$(new_repo exempt)"
printf '%s\n' 'printf "resolve node"' \
  >>"$repo/plugins/ostrom/scripts/run-node.sh"
commit_all "$repo"
exempt_output="$(cd "$repo" && bash "$guard" base HEAD 2>&1)"
grep -Fq 'permanently exempt from the retirement' <<<"$exempt_output"

# The exemption is per-path, not a blanket amnesty: a non-exempt script that
# grows in the same commit must still fail, and must be the only one named.
repo="$(new_repo exempt_mixed)"
printf '%s\n' 'printf "resolve node"' \
  >>"$repo/plugins/ostrom/scripts/run-node.sh"
printf '%s\n' 'printf delta' \
  >>"$repo/plugins/ostrom/scripts/mixed-grow.sh"
commit_all "$repo"
set +e
exempt_mixed_output="$(cd "$repo" && bash "$guard" base HEAD 2>&1)"
exempt_mixed_status=$?
set -e
[[ "$exempt_mixed_status" -eq 1 ]]
grep -Fq 'implement this in Rust' <<<"$exempt_mixed_output"
grep -Fq 'plugins/ostrom/scripts/mixed-grow.sh' <<<"$exempt_mixed_output"
grep -Fq 'run-node.sh grew by 1 lines (2 -> 3); permanently exempt' <<<"$exempt_mixed_output"

echo "shell freeze tests: ok"
