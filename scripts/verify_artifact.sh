#!/usr/bin/env bash

set -euo pipefail

artifact="${1:-target/release/libmongodb_vs.so}"
# Optional release platform the artifact must be built for, such as linux-x86_64.
platform="${2:-}"

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

description="$(file -b "$artifact")"
[[ "$description" == "ELF 64-bit"* ]] || {
  echo "error: artifact is not a 64-bit Linux ELF shared object" >&2
  exit 1
}

case "$description" in
  *x86-64*) detected_platform="linux-x86_64" ;;
  *aarch64*) detected_platform="linux-aarch64" ;;
  *)
    echo "error: artifact targets an unsupported architecture: $description" >&2
    exit 1
    ;;
esac
[[ -z "$platform" || "$platform" == "$detected_platform" ]] || {
  echo "error: artifact is built for $detected_platform, not $platform" >&2
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
if ! sdk_version="$(
  awk '
    $0 == "[[package]]" { is_sdk = 0 }
    $0 == "name = \"exasol-udf-sdk\"" { is_sdk = 1 }
    is_sdk && /^version = "/ {
      version = $3
      gsub(/\"/, "", version)
      versions[++count] = version
      is_sdk = 0
    }
    END {
      if (count != 1) exit 1
      print versions[1]
    }
  ' "$project_dir/Cargo.lock"
)"; then
  echo "error: Cargo.lock must contain exactly one exasol-udf-sdk package" >&2
  exit 1
fi

toolchain_fingerprint="$(tr -d '\r\n' < "$project_dir/rust-udf-fingerprint.txt")"
[[ "$toolchain_fingerprint" == rustc_* ]] || {
  echo "error: rust-udf-fingerprint.txt must contain the Rust SLC toolchain fingerprint" >&2
  exit 1
}

expected_fingerprint="${sdk_version}:${toolchain_fingerprint}"
grep -aFq "$expected_fingerprint" "$artifact" || {
  echo "error: artifact does not contain the required Rust SLC fingerprint" >&2
  printf 'expected: %s\n' "$expected_fingerprint" >&2
  exit 1
}

echo "Artifact verified: $detected_platform ELF, expected entry points, SLC fingerprint $expected_fingerprint."
