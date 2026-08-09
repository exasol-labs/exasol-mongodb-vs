#!/usr/bin/env bash

set -euo pipefail

artifact="${1:-target/release/libmongodb_vs.so}"

for command in file nm; do
  command -v "$command" >/dev/null || {
    echo "error: required command not found: $command" >&2
    exit 1
  }
done

[[ -f "$artifact" ]] || {
  echo "error: UDF artifact missing: $artifact" >&2
  exit 1
}

file "$artifact" | grep -q 'ELF 64-bit' || {
  echo "error: artifact is not a 64-bit Linux ELF shared object" >&2
  exit 1
}

entry_points="$(nm -D --defined-only "$artifact" | awk '/__exa_udf_entry_/ {print $3}' | sort | paste -sd ' ' -)"
expected="__exa_udf_entry_MONGODB_ADAPTER __exa_udf_entry_MONGODB_SCAN"

[[ "$entry_points" == "$expected" ]] || {
  echo "error: UDF exports differ from the expected adapter and scan entry points" >&2
  printf 'found: %s\n' "${entry_points:-(none)}" >&2
  exit 1
}

project_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
expected_fingerprint="$(tr -d '\r\n' < "$project_dir/rust-udf-fingerprint.txt")"
grep -aFq "$expected_fingerprint" "$artifact" || {
  echo "error: artifact does not contain the required Rust SLC fingerprint" >&2
  printf 'expected: %s\n' "$expected_fingerprint" >&2
  exit 1
}

echo "Artifact verified: Linux ELF, expected entry points, SLC fingerprint $expected_fingerprint."
