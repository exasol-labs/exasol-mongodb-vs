#!/usr/bin/env bash
# Write SHA256SUMS over every platform bundle of one release in a directory, so
# packages built separately per platform share a single checksum manifest.

set -euo pipefail

requested="${1:-}"
output_dir="${2:-target/release-package}"
version="${requested#v}"

[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]] || {
  echo "error: invalid release version '$requested'" >&2
  exit 1
}

cd "$output_dir"
shopt -s nullglob
files=("exasol-mongodb-vs-$version-"*.tar.gz "exasol-mongodb-vs-$version-"*.so)
((${#files[@]} > 0)) || {
  echo "error: no release bundles for $version in $output_dir" >&2
  exit 1
}

if command -v sha256sum >/dev/null; then
  LC_ALL=C sha256sum "${files[@]}" >SHA256SUMS
else
  LC_ALL=C shasum -a 256 "${files[@]}" >SHA256SUMS
fi
