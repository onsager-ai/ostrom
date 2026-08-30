#!/bin/sh
set -eu

path=$1
kind=$2

case "$kind" in
  manifest)
    sed -i 's/Writes work orders/Writes changed work orders/' "$path"
    ;;
  prompt)
    printf '\nChanged after signing.\n' >>"$path"
    ;;
  *)
    exit 2
    ;;
esac
