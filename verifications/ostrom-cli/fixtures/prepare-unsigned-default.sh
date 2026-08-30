#!/bin/sh
set -eu

ostrom_bin=$1
scratch_root=$2
case_name=$3
case_dir="$scratch_root/$case_name"

case "$case_dir" in
  target/duhem-ostrom-cli/*) ;;
  *) exit 2 ;;
esac
rm -rf -- "$case_dir"
mkdir -p "$case_dir/trusted"
OSTROM_HOME="$case_dir" "$ostrom_bin" init >/dev/null
