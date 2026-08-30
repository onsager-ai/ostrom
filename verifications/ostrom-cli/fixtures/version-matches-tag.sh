#!/bin/sh
set -eu

ostrom_bin=$1
tag=$(git describe --tags --exact-match HEAD)
tag_version=${tag#v}
binary_version=$("$ostrom_bin" --version)
binary_version=${binary_version#ostrom }
test "$binary_version" = "$tag_version"
