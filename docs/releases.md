# Releases and runner maintenance

The CLI is distributed through crates.io. Runner images are distributed through GHCR using the matching CLI tag (`ghcr.io/clroot/runnerctl-runner:v0.3.0`). No downloadable CLI binaries are produced.

## Publish

1. Update `Cargo.toml` and `Cargo.lock` to the intended version and prepare release notes. Check the development install and quick start.
2. Run formatting, clippy, unit/CLI tests, Docker lifecycle/entrypoint tests, `python3 scripts/update-runner.py --check`, and the workflow compatibility suite. Review `cargo package --list` and perform `cargo publish --dry-run --locked`.
3. Commit the release and publish a non-prerelease GitHub release for the matching `v<version>` tag.
4. The publish workflow validates the tag and tests, builds/pushes Linux amd64 and arm64 images, and only then publishes the crate using `CARGO_REGISTRY_TOKEN`.
5. The final publish job checks anonymous image access before publishing the crate. On the first image publication, make the GHCR package public in package settings, then rerun that job if the check failed. The OCI source label links it to this repository. Check that the published image contains the expected `io.runnerctl.runner.version` label.

GHCR requires `packages: write` for the image workflow. The crates.io token must have publishing rights for `runnerctl`. Image build or validation failure prevents crate publication. A crates.io failure after image publication can be retried without changing the release source.

## Upstream updates

`runner/version.json` records the upstream version, release date, and architecture archive digests. `scripts/update-runner.py` fetches the latest stable release from `actions/runner`, requires GitHub-provided SHA-256 asset digests, and updates the manifest and Dockerfile together. `--check` validates committed consistency offline.

The daily `Check runner updates` workflow opens or updates the `automation/runner-version` PR. Repository Actions settings must allow GitHub Actions to create pull requests. Review and merge the update, bump the CLI patch version, and publish matching images promptly. This workflow does not automatically merge or publish an unreviewed upstream update.

Automation using `GITHUB_TOKEN` has [workflow triggering restrictions](https://docs.github.com/en/actions/how-tos/write-workflows/choose-when-workflows-run/trigger-a-workflow): push events do not start another workflow, and generated PR runs can require approval. Verify CI actually ran before merging; approve the pending run or manually dispatch CI against the update branch.

GitHub normally requires runner updates within 30 days of a new release; critical security updates may block job assignment sooner. `doctor` provides an additional local version check, not a substitute for release monitoring.
