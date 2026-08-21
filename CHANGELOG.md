# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
