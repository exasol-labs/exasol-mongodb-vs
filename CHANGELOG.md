# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed

- `WHERE 1 = 0` and every other predicate Exasol folds to a boolean literal —
  `WHERE FALSE`, `WHERE 2 > 3`, `WHERE 1 = 0 AND "status" = 'x'` — no longer fail
  with `filter contains an operation that was not advertised`. `LITERAL_BOOL` is
  advertised but `parse_filter` had no arm for a `literal_bool` node, so the
  standard JDBC/ODBC and BI metadata probe made every table of a virtual schema
  unopenable. A folded boolean literal is now a first-class filter: constant-false
  returns no rows without resolving a connection or querying MongoDB, and
  constant-true is dropped so `LIMIT` and `ORDER BY` delegation is unaffected.
  Constants nested in `AND`, `OR`, and `NOT` are folded on arrival.
- The unsupported-filter error now names the node type it could not plan
  (`filter contains an unsupported node type 'predicate_like'`) instead of
  claiming the operation was not advertised, which was misleading for anything
  the capability set does advertise.
- The `adapterNotes` version check is now reachable for the case it exists for.
  Notes written before a field became required failed the whole-struct parse
  first, so every statement against such a schema reported only the generic
  `pushdown adapterNotes are invalid`. The version is now read before the body,
  and the error names the version found and the exact remedy,
  `ALTER VIRTUAL SCHEMA "<schema>" REFRESH`.

### Changed

- Moved the fingerprinted pins to Rust Script Language Container 0.23.0:
  `exasol-udf-sdk` and `exasol-udf-macros` are now pinned to `=0.23.0`, so the
  required fingerprint is `0.23.0:rustc_1.94.1__e408947bf_2026-03-25_`. The
  Rust toolchain pin is unchanged.
- Moved the supported artifact build from `rust:1.94.1-bookworm` to
  `rust:1.94.1-trixie`, matching the Debian release SLC 0.23.0 stages its
  runtime tree from (glibc floor 2.41), so the artifact links against the same
  glibc it loads against instead of an older one.

## [0.1.0]

### Added

- MongoDB Virtual Schema discovery from validators, indexes, and deterministic
  sampling.
- Relational modeling for nested objects, arrays, BSON scalar variants, and
  complete source-document JSON through `TO_JSON()` interoperability.
- Conservative filter, projection, limit, top-N, and count pushdown.
- Unit, property, integration, coverage, optional fuzzing, and live Exasol test
  suites.

### Fixed

- Adversarial schema-inference defects AVS-001 through AVS-006, including
  structural unions, mixed arrays, deterministic sampling, index evidence, and
  bounded Exasol identifiers.
- Double-typed predicates now receive exact, BSON-type-guarded MongoDB pushdown
  instead of transferring the full collection for Exasol-side filtering.
- Every advertised predicate capability is covered by a source-delegation
  contract test, and direct `>` / `>=` predicates are now advertised and pushed.
- Documented safe row-level aggregation across polymorphic numeric branches,
  including finite-double validation and non-finite-value accounting.
