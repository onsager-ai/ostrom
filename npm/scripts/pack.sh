#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
staging="$root/target/npm-packages"
output="$root/target/npm-tarballs"

while [ "$#" -gt 0 ]; do
  case "$1" in
    --staging) staging="$(realpath -m "$2")"; shift 2 ;;
    --output) output="$(realpath -m "$2")"; shift 2 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

case "$output" in
  "$root"/target/*) ;;
  *) echo "refusing unsafe output path: $output" >&2; exit 2 ;;
esac
rm -rf -- "$output"
mkdir -p "$output"

index=0
while IFS=$'\t' read -r package_dir package_name; do
  echo "packing $package_name"
  npm_config_cache="$output/.npm-cache" npm pack "$package_dir" \
    --ignore-scripts \
    --json \
    --pack-destination "$output" > "$output/.pack-result-$index.json"
  index=$((index + 1))
done < <(node "$root/npm/scripts/package-list.mjs" --staging "$staging")

node "$root/npm/scripts/verify-pack-output.mjs" \
  --staging "$staging" \
  --output "$output"
