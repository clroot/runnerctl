# Contributing

Open an issue with the runnerctl version, host OS/architecture, Docker version, image tag, relevant pool settings, and a minimal workflow that reproduces the issue. Remove credentials and sensitive job output from logs.

Run `cargo fmt --check`, `cargo clippy --all-targets --locked -- -D warnings`, and `cargo test --locked` before submitting a change. Changes to runner lifecycle, container setup, or authentication should also run the applicable [Docker and GitHub checks](tests/README.md).

Use a dedicated `--home`, unique labels, and test repositories for real GitHub validation. Never reuse a production pool for destructive lifecycle tests. Explain the behavior change and verification in your pull request. Configuration and saved state from earlier releases should remain readable unless an explicit migration is provided.
