# Development and testing

## Toolchain

The repository pins Rust 1.94.1 in `rust-toolchain.toml` and the fingerprinted
Exasol UDF SDK and macro crates to 0.22.1. Install the
additional quality tools with:

```bash
cargo install cargo-llvm-cov --version 0.8.4 --locked
cargo install cargo-deny --version 0.20.2 --locked
brew install shellcheck # or use your platform package manager
```

Docker is required for MongoDB integration tests and the supported Linux UDF
build. The live end-to-end test also requires an Exasol Personal deployment and
its `exasol` command-line client.

## Local checks

```bash
make test       # Rust unit tests
make check      # formatting and strict Clippy
make quality    # complete pre-merge gate
```

`make quality` runs locked-dependency builds, formatting, Clippy with all
warnings denied, ShellCheck, dependency advisory/license/source policy, and all
tests under LLVM coverage. It enforces at least 85% line and region coverage and
75% function coverage. The per-file summary is printed in the gate log, and
LCOV output is written to `target/coverage/lcov.info` before thresholds are
evaluated so failed coverage gates remain diagnosable.

The GitHub Actions workflow runs the same quality gate, MongoDB inference
integration tests, and an independent Linux artifact check. Workflow actions and
auxiliary Rust tools are pinned.

## Linux UDF artifact

```bash
make build-so
make verify-so
```

The build runs in the pinned Debian/glibc container compatible with the Rust
Script Language Container and creates:

```text
target/release/libmongodb_vs.so
```

The verifier checks that the result is a 64-bit Linux ELF exporting exactly
`MONGODB_ADAPTER` and `MONGODB_SCAN`. It derives the SDK version from
`Cargo.lock` and combines it with the SLC toolchain fingerprint recorded in
`rust-udf-fingerprint.txt`, so an SDK upgrade cannot leave a stale expected
version in the artifact check. A host-built release library is not a supported
deployment artifact.

## MongoDB integration test

```bash
make test-integration
```

This starts an authenticated MongoDB 8 container and verifies validator, index,
and sample inference, deterministic manifests/fingerprints, and restricted
metadata permissions.

## Live MongoDB-to-Exasol test

```bash
make test-e2e
```

The script targets the Exasol Personal deployment at
`$HOME/.exasol/personal/deployments/default` unless overridden. It creates
run-scoped MongoDB, database, collection, connection, and Virtual Schema names,
then removes them.

It verifies:

- explicit and inferred table families;
- validators, indexes, nested objects, and nested arrays;
- variants and missing/null/empty-string behavior;
- stable joins and byte-identical inferred refresh;
- conservative `AND`/`OR`/`NOT` filter, limit, top-N, and single-group `COUNT(*)` pushdown;
- parity against a no-capability Virtual Schema;
- selected-field schema drift failures; and
- credential-free `EXPLAIN VIRTUAL` output.

Useful overrides:

```bash
EXASOL_DEPLOYMENT_DIR=/path/to/deployment \
MONGODB_E2E_GATEWAY=192.168.64.1 \
MONGODB_E2E_SKIP_BUILD=1 \
MONGODB_E2E_KEEP_RESOURCES=1 \
make test-e2e
```

The live suite is an explicit release gate rather than a normal CI job because
it requires an Exasol deployment.
