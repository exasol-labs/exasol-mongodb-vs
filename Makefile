COVERAGE_MIN_LINES ?= 90
COVERAGE_MIN_FUNCTIONS ?= 85
COVERAGE_MIN_REGIONS ?= 90
COVERAGE_REPORT ?= target/coverage/lcov.info
FUZZ_TOOLCHAIN ?= nightly-2026-08-01
FUZZ_MAX_TOTAL_TIME ?= 300
FUZZ_MAX_LEN ?= 65536
FUZZ_TIMEOUT ?= 10
FUZZ_RSS_LIMIT_MB ?= 2048
FUZZ_TARGETS := manifest_parse scan_spec_parse pushdown_plan

.PHONY: test property-tests check quality fmt-check lint-rust lint-shell dependencies coverage \
	build-so verify-so test-integration test-e2e fuzz-replay fuzz-release \
	verify-release-version package-release

# Fast developer loop. Coverage deliberately owns the authoritative test run in
# `quality`, so the full gate does not execute the suite twice.
test:
	cargo test --locked --workspace --all-features

# Run generated cases outside LLVM instrumentation so macro-expanded test
# harnesses cannot inflate production coverage percentages.
property-tests:
	cargo test --locked --workspace --all-features property_tests::

fmt-check:
	cargo fmt --all -- --check

lint-rust:
	cargo clippy --locked --workspace --all-targets --all-features -- -D warnings

lint-shell:
	@command -v shellcheck >/dev/null || { echo "error: shellcheck is required" >&2; exit 1; }
	shellcheck scripts/*.sh

dependencies:
	@command -v cargo-deny >/dev/null || { echo "error: cargo-deny is required" >&2; exit 1; }
	cargo deny check

check: fmt-check lint-rust

coverage:
	@command -v cargo-llvm-cov >/dev/null || { echo "error: cargo-llvm-cov is required" >&2; exit 1; }
	@mkdir -p $(dir $(COVERAGE_REPORT))
	cargo llvm-cov clean --workspace
	cargo llvm-cov --locked --workspace --all-features --no-report
	cargo llvm-cov report --lcov --output-path $(COVERAGE_REPORT)
	cargo llvm-cov report --summary-only \
		--fail-under-lines $(COVERAGE_MIN_LINES) \
		--fail-under-functions $(COVERAGE_MIN_FUNCTIONS) \
		--fail-under-regions $(COVERAGE_MIN_REGIONS)

# Reproducible pre-merge gate: source lint, shell lint, dependency policy, tests,
# and coverage regression protection.
quality: check lint-shell dependencies property-tests coverage

# Fuzzing is an explicit release activity. It uses its own nightly workspace and
# is deliberately not a dependency of the stable, deterministic quality gate.
fuzz-replay:
	@command -v cargo-fuzz >/dev/null || { echo "error: cargo-fuzz is required (cargo install cargo-fuzz --version 0.13.2 --locked)" >&2; exit 1; }
	@for target in $(FUZZ_TARGETS); do \
		echo "replaying fuzz corpus: $$target"; \
		(cd fuzz && cargo +$(FUZZ_TOOLCHAIN) fuzz run $$target corpus/$$target -- -runs=0 -max_len=$(FUZZ_MAX_LEN) -dict=dictionaries/$$target.dict) || exit $$?; \
	done

fuzz-release:
	@command -v cargo-fuzz >/dev/null || { echo "error: cargo-fuzz is required (cargo install cargo-fuzz --version 0.13.2 --locked)" >&2; exit 1; }
	@for target in $(FUZZ_TARGETS); do \
		echo "fuzzing $$target for up to $(FUZZ_MAX_TOTAL_TIME) seconds"; \
		(cd fuzz && cargo +$(FUZZ_TOOLCHAIN) fuzz run $$target corpus/$$target -- -max_total_time=$(FUZZ_MAX_TOTAL_TIME) -max_len=$(FUZZ_MAX_LEN) -timeout=$(FUZZ_TIMEOUT) -rss_limit_mb=$(FUZZ_RSS_LIMIT_MB) -dict=dictionaries/$$target.dict) || exit $$?; \
	done

# Build in the same Debian release the Rust SLC stages its runtime from. SLC
# 0.23.0 builds its client on rust:1.94-trixie and donates that tree's glibc
# (floor 2.41), so trixie is the exact target environment: the artifact links
# against the same glibc it will load against, and `cargo exasol-udf validate`
# accepts glibc references up to that floor. A host release artifact is not a
# supported Exasol deployment artifact.
build-so:
	docker run --rm -v "$(CURDIR):/build" -w /build rust:1.94.1-trixie \
		bash -c 'apt-get update -qq && apt-get install -y -qq protobuf-compiler pkg-config cmake && cargo build --locked --release -p mongodb-vs'

verify-so:
	./scripts/verify_artifact.sh target/release/libmongodb_vs.so

verify-release-version:
	@test -n "$(VERSION)" || { echo "error: VERSION is required" >&2; exit 1; }
	./scripts/verify_release_version.sh "$(VERSION)"

package-release: verify-so verify-release-version
	./scripts/package_release.sh "$(VERSION)" "$(RELEASE_OUTPUT_DIR)" "$(RELEASE_PLATFORM)"

test-integration:
	./scripts/run_mongodb_integration.sh

test-e2e:
	./scripts/run_e2e.sh
