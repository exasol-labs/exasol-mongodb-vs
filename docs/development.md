# Development and testing

## Toolchain

The repository pins Rust 1.94.1 in `rust-toolchain.toml` and the fingerprinted
Exasol UDF SDK and macro crates to 0.23.0. Install the additional quality tools
with:

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
tests under LLVM coverage. It enforces at least 90% line and region coverage and
85% function coverage. The per-file summary is printed in the gate log, and
LCOV output is written to `target/coverage/lcov.info` before thresholds are
evaluated so failed coverage gates remain diagnosable.

Core inference, pushdown, BSON conversion, SQL quoting, and wire-format
invariants also run as bounded `proptest` cases within the normal Rust test
suite. The quality gate runs these separately from LLVM instrumentation so test
harness code cannot inflate production coverage. A failing case is
automatically reduced to a minimal reproducible input.

## Optional release fuzzing

Before a major release, run the coverage-guided parser and planner campaigns:

```bash
cargo install cargo-fuzz --version 0.13.2 --locked # first use only
make fuzz-replay
make fuzz-release
```

`fuzz-replay` deterministically checks every committed corpus entry.
`fuzz-release` then runs each target for five minutes by default (15 minutes in
total). Override the per-target duration when appropriate, for example:

```bash
FUZZ_MAX_TOTAL_TIME=1800 make fuzz-release
```

The targets exercise explicit-manifest parsing, scan-spec parsing and
round-tripping, and pushdown planning/rendering. Inputs are capped at 64 KiB to
keep mutation throughput high; deterministic unit tests continue to own the
larger protocol-size boundaries. Target-specific dictionaries help libFuzzer
preserve the JSON protocol vocabulary while mutating structure and values.

Fuzzing has an independent workspace and a pinned nightly toolchain because
`cargo-fuzz`/libFuzzer require nightly compiler instrumentation. It is
intentionally not part of `make quality`, normal CI, or coverage calculations.
When a campaign finds a failure, reproduce it with the command printed by
`cargo-fuzz`, add a focused unit regression test, and retain a minimized corpus
input when it reaches a distinct path.

The GitHub Actions workflow runs the same quality gate, MongoDB inference
integration tests, and an independent Linux artifact check. Workflow actions and
auxiliary Rust tools are pinned.

Release candidates and draft releases use separate workflows. See
[Release process](releasing.md) for versioning, artifact, provenance, live-test,
and manual publication gates.

## Linux UDF artifact

```bash
make build-so
make verify-so
```

The build runs in `rust:1.94.1-trixie`, matching the Debian release the Rust
Script Language Container stages its own runtime tree from, and creates:

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
