#!/usr/bin/env bash

set -Eeuo pipefail

repo_root="$(git rev-parse --show-toplevel)"
resolver="$repo_root/plugins/ostrom/tests/resolve-msrv.sh"
test_root="$(mktemp -d)"
trap 'rm -rf -- "$test_root"' EXIT

manifest="$test_root/Cargo.toml"

write_manifest() {
  local version="$1"
  printf '[workspace]\n\n[workspace.package]\nrust-version = "%s"\n' "$version" \
    >"$manifest"
}

write_manifest 1.86
resolved="$(bash "$resolver" "$manifest")"
grep -Fxq 'declared=1.86' <<<"$resolved"
grep -Fxq 'toolchain=1.86.0' <<<"$resolved"

# A second value proves the result follows the manifest rather than a fallback
# that merely agrees with the repository today.
write_manifest 1.77
resolved="$(bash "$resolver" "$manifest")"
grep -Fxq 'declared=1.77' <<<"$resolved"
grep -Fxq 'toolchain=1.77.0' <<<"$resolved"

write_manifest 1.86.1
resolved="$(bash "$resolver" "$manifest")"
grep -Fxq 'declared=1.86.1' <<<"$resolved"
grep -Fxq 'toolchain=1.86.1' <<<"$resolved"

printf '[workspace]\n\n[workspace.package]\n' >"$manifest"
set +e
missing_output="$(bash "$resolver" "$manifest" 2>&1)"
missing_status=$?
set -e
[[ "$missing_status" -ne 0 ]]
grep -Fq 'missing rust-version' <<<"$missing_output"

write_manifest next
set +e
malformed_output="$(bash "$resolver" "$manifest" 2>&1)"
malformed_status=$?
set -e
[[ "$malformed_status" -ne 0 ]]
grep -Fq "invalid rust-version 'next'" <<<"$malformed_output"

workflow_toolchain_literal='cargo[[:space:]]+\+[0-9]+\.[0-9]+|rustup[[:space:]]+toolchain[[:space:]]+install[[:space:]]+[0-9]+\.[0-9]+|toolchain:[[:space:]]*["'\'' ]*[0-9]+\.[0-9]+'
if grep -R -n -E "$workflow_toolchain_literal" "$repo_root/.github/workflows"; then
  echo "msrv tests: workflow hardcodes a Rust toolchain version" >&2
  exit 1
fi

repository_resolution="$(bash "$resolver" "$repo_root/Cargo.toml")"
repository_toolchain="$(sed -n 's/^toolchain=//p' <<<"$repository_resolution")"
if grep -R -n -F "$repository_toolchain" "$repo_root/.github/workflows"; then
  echo "msrv tests: workflow copies the toolchain resolved from Cargo.toml" >&2
  exit 1
fi

for workflow in test.yml release.yml; do
  grep -Fq 'plugins/ostrom/tests/resolve-msrv.sh Cargo.toml' \
    "$repo_root/.github/workflows/$workflow"
done

echo "msrv tests: ok"
