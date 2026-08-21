# Release process

Releases use two GitHub Actions workflows. Neither workflow publishes directly.
The tag workflow creates a draft release that must be reviewed and published
manually.

## One-time repository settings

Protect `main` and require these checks before merge:

- `Lint, test, and coverage`
- `Linux UDF artifact`
- `MongoDB inference integration`

Enable the merge queue if the repository uses queued merges; CI supports the
`merge_group` event. Create a `release` environment, restrict it to version tags
matching `v*`, and require at least one reviewer. Keep the default workflow
token read-only; the draft-release job receives narrowly scoped write,
attestation, and OIDC permissions.

## Prepare a candidate

1. Update the workspace version in `Cargo.toml` and regenerate `Cargo.lock`.
2. Add an exact `## [VERSION]` heading to `CHANGELOG.md`.
3. Confirm `rust-udf-fingerprint.txt` and the exact SDK pins describe the target
   Rust Script Language Container.
4. Run the `Release candidate` workflow manually with that version. Fuzzing is
   opt-in and remains outside normal CI; enable it for major-release campaigns
   when the execution environment has sufficient resources.
5. Download the retained candidate artifact and run `make test-e2e` against a
   matching Exasol deployment. Record the successful workflow and live-test
   run in the release review.

The candidate workflow validates version metadata, runs the quality and
MongoDB integration gates, builds the supported Linux UDF in the pinned
container, verifies its entry points and SLC fingerprint, and uploads a bundle,
standalone shared library, and `SHA256SUMS`. It does not create a tag or release.

## Create the draft release

After candidate and live Exasol validation, create and push an annotated tag:

```bash
git tag -s vVERSION -m "exasol-mongodb-vs vVERSION"
git push origin vVERSION
```

The `Draft release` workflow rebuilds and revalidates the tagged source,
generates checksummed packages, records GitHub build-provenance attestations,
and creates a draft GitHub release with generated notes. Review the draft,
checksums, attestation, changelog, and recorded live-test evidence before using
GitHub's **Publish release** action.

No crates.io publication occurs; the crate has `publish = false`.

Dependabot deliberately ignores `exasol-udf-sdk` and `exasol-udf-macros`.
Those exact pins must move together with an explicitly selected SLC; ordinary
Cargo and GitHub Actions dependencies continue to receive grouped weekly update
pull requests.
