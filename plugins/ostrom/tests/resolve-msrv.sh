#!/usr/bin/env bash

set -Eeuo pipefail

manifest="${1:-Cargo.toml}"

if [[ ! -f "$manifest" ]]; then
  echo "msrv resolver: manifest not found: $manifest" >&2
  exit 1
fi

if ! declared="$({
  awk '
    function fail(message) {
      print "msrv resolver: " message > "/dev/stderr"
      failed = 1
      exit 1
    }

    /^[[:space:]]*\[workspace\.package\][[:space:]]*(#.*)?$/ {
      in_workspace_package = 1
      next
    }

    /^[[:space:]]*\[/ {
      in_workspace_package = 0
      next
    }

    in_workspace_package && /^[[:space:]]*rust-version[[:space:]]*=/ {
      if (found) {
        fail("duplicate rust-version in [workspace.package]")
      }

      value = $0
      sub(/^[[:space:]]*rust-version[[:space:]]*=[[:space:]]*/, "", value)
      if (value !~ /^"[^"]*"[[:space:]]*(#.*)?$/) {
        fail("rust-version in [workspace.package] must be one quoted version")
      }

      sub(/^"/, "", value)
      sub(/"[[:space:]]*(#.*)?$/, "", value)
      print value
      found = 1
    }

    END {
      if (!failed && !found) {
        fail("missing rust-version in [workspace.package]")
      }
    }
  ' "$manifest"
})"; then
  exit 1
fi

version_component='(0|[1-9][0-9]*)'
if [[ ! "$declared" =~ ^${version_component}\.${version_component}(\.${version_component})?$ ]]; then
  echo "msrv resolver: invalid rust-version '$declared' in $manifest; expected major.minor or major.minor.patch" >&2
  exit 1
fi

if [[ "$declared" == *.*.* ]]; then
  toolchain="$declared"
else
  toolchain="$declared.0"
fi

printf 'declared=%s\ntoolchain=%s\n' "$declared" "$toolchain"
