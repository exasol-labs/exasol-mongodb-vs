#!/usr/bin/env bash

set -euo pipefail

requested="${1:-}"
version="${requested#v}"

[[ "$requested" == "$version" || "$requested" == "v$version" ]] || {
  echo "error: release version must be VERSION or vVERSION" >&2
  exit 1
}
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]] || {
  echo "error: release version must be semantic (for example 1.2.3 or 1.2.3-rc.1)" >&2
  exit 1
}

project_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
workspace_version="$({
  cargo metadata --locked --no-deps --format-version 1
} | python3 -c '
import json
import sys

packages = [p for p in json.load(sys.stdin)["packages"] if p["name"] == "mongodb-vs"]
if len(packages) != 1:
    raise SystemExit("expected exactly one mongodb-vs package")
print(packages[0]["version"])
')"

[[ "$version" == "$workspace_version" ]] || {
  echo "error: requested release $version does not match Cargo package version $workspace_version" >&2
  exit 1
}

grep -Fqx "## [$version]" "$project_dir/CHANGELOG.md" || {
  echo "error: CHANGELOG.md requires an exact '## [$version]' release heading" >&2
  exit 1
}

echo "Release version verified: $version"
