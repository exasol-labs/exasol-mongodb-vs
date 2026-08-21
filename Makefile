COVERAGE_MIN_LINES ?= 85
COVERAGE_MIN_FUNCTIONS ?= 75
COVERAGE_MIN_REGIONS ?= 85
COVERAGE_REPORT ?= target/coverage/lcov.info

.PHONY: test check quality fmt-check lint-rust lint-shell dependencies coverage \
	build-so verify-so test-integration test-e2e

# Fast developer loop. Coverage deliberately owns the authoritative test run in
# `quality`, so the full gate does not execute the suite twice.
test:
	cargo test --locked --workspace --all-features

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
	cargo llvm-cov --locked --workspace --all-features --no-report
	cargo llvm-cov report --lcov --output-path $(COVERAGE_REPORT)
	cargo llvm-cov report --summary-only \
		--fail-under-lines $(COVERAGE_MIN_LINES) \
		--fail-under-functions $(COVERAGE_MIN_FUNCTIONS) \
		--fail-under-regions $(COVERAGE_MIN_REGIONS)

# Reproducible pre-merge gate: source lint, shell lint, dependency policy, tests,
# and coverage regression protection.
quality: check lint-shell dependencies coverage

# Build in the same Debian/glibc environment as the working Rust SLC. A host
# release artifact is not a supported Exasol deployment artifact.
build-so:
	docker run --rm -v "$(CURDIR):/build" -w /build rust:1.94.1-bookworm \
		bash -c 'apt-get update -qq && apt-get install -y -qq protobuf-compiler pkg-config cmake && cargo build --locked --release -p mongodb-vs'

verify-so:
	./scripts/verify_artifact.sh target/release/libmongodb_vs.so

test-integration:
	./scripts/run_mongodb_integration.sh

test-e2e:
	./scripts/run_e2e.sh
