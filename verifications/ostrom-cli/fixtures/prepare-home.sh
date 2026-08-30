#!/bin/sh
set -eu

scratch_root=$1
case_name=$2
case_dir="$scratch_root/$case_name"

case "$case_dir" in
  target/duhem-ostrom-cli/*) ;;
  *) exit 2 ;;
esac
if [ -d "$case_dir" ]; then
  chmod -R u+w -- "$case_dir"
fi
rm -rf -- "$case_dir"
mkdir -p "$case_dir/trusted" "$case_dir/empty-trust"
openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:2048 \
  -out "$case_dir/private.pem" 2>/dev/null
openssl pkey -in "$case_dir/private.pem" -pubout \
  -out "$case_dir/trusted/vd-principal.pem" 2>/dev/null
