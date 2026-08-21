#!/usr/bin/env bash

set -euo pipefail

requested="${1:-}"
output_dir="${2:-target/release-package}"
platform="${3:-linux-x86_64}"
version="${requested#v}"

[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]] || {
  echo "error: invalid release version '$requested'" >&2
  exit 1
}
[[ "$platform" =~ ^[a-z0-9][a-z0-9_-]*$ ]] || {
  echo "error: invalid release platform '$platform'" >&2
  exit 1
}

project_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
artifact="$project_dir/target/release/libmongodb_vs.so"
bundle="exasol-mongodb-vs-$version-$platform"
archive="$output_dir/$bundle.tar.gz"
standalone="$output_dir/$bundle.so"
checksums="$output_dir/SHA256SUMS"
stage="$(mktemp -d)"
trap 'rm -rf "$stage"' EXIT

"$project_dir/scripts/verify_release_version.sh" "$version"
"$project_dir/scripts/verify_artifact.sh" "$artifact"

mkdir -p "$output_dir" "$stage/$bundle/sql"
install -m 0755 "$artifact" "$standalone"
install -m 0755 "$artifact" "$stage/$bundle/libmongodb_vs.so"
install -m 0644 "$project_dir/sql/install.sql" "$stage/$bundle/sql/install.sql"
install -m 0644 "$project_dir/README.md" "$stage/$bundle/README.md"
install -m 0644 "$project_dir/CHANGELOG.md" "$stage/$bundle/CHANGELOG.md"
install -m 0644 "$project_dir/LICENSE" "$stage/$bundle/LICENSE"
install -m 0644 "$project_dir/Cargo.lock" "$stage/$bundle/Cargo.lock"
install -m 0644 "$project_dir/rust-udf-fingerprint.txt" "$stage/$bundle/rust-udf-fingerprint.txt"

tar --sort=name --mtime="@${SOURCE_DATE_EPOCH:-0}" --owner=0 --group=0 \
  --numeric-owner -czf "$archive" -C "$stage" "$bundle"

if command -v sha256sum >/dev/null; then
  (cd "$output_dir" && sha256sum "$(basename "$archive")" "$(basename "$standalone")") >"$checksums"
else
  (cd "$output_dir" && shasum -a 256 "$(basename "$archive")" "$(basename "$standalone")") >"$checksums"
fi

echo "Release package prepared in $output_dir"
