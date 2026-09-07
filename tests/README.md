# Tests

`cargo test --locked` runs config, state recovery, API mock, CLI and Unix socket tests without real credentials or runner registration.

For Docker lifecycle checks:

```bash
docker build -t local/runnerctl-runner:2.337.0 runner
docker build -t local/runnerctl-test:1 -f tests/Dockerfile .
bash tests/entrypoint.sh
cargo test --locked docker_replacement_and_restart_recovery -- --ignored --nocapture
```

The test image uses a local completion stub and never connects to GitHub. The manager uses a loopback HTTP fake for registration/list/delete. Containers are labeled with a randomly generated manager ID and cleaned up by the test guard. The test verifies two independent pools, job completion/replacement, manager state recovery and persistence of a stopped pool.

The entrypoint smoke test uses the production image with stubbed config/run scripts and networking disabled. It checks non-root execution, short-lived token consumption, ephemeral/update flags and completion hook permissions.

An end-to-end GitHub job requires a separately configured test organization and token. This is deliberately separate from the default test suite.
