#!/bin/sh
set -eu

ostrom_bin=$1
scratch_root=$2
case_name=$3
case_dir="$scratch_root/$case_name"

sh verifications/ostrom-cli/fixtures/prepare-home.sh "$scratch_root" "$case_name"
OSTROM_HOME="$case_dir" "$ostrom_bin" init >/dev/null
"$ostrom_bin" sign --key-id vd-principal --key "$case_dir/private.pem" \
  "$case_dir/ostrom.yaml" >/dev/null
