# Tests

`cargo test --locked` runs config, state recovery, API mock, CLI and Unix socket tests without real credentials or runner registration.

For Docker lifecycle checks:

```bash
docker build -t local/runnerctl-test-base:1 runner
docker build -t local/runnerctl-test:1 -f tests/Dockerfile .
bash tests/entrypoint.sh
bash tests/docker-socket.sh
cargo test --locked docker_ -- --ignored --nocapture
```

The test image uses a local completion stub and never connects to GitHub. The manager uses a loopback HTTP fake for registration/list/delete. Containers are labeled with a randomly generated manager ID and cleaned up by the test guard. The tests verify two independent pools, job completion/replacement, manager state recovery, persistence of a stopped pool, batch exit recovery during provisioning backoff without interrupting busy work, reading the Docker host boot time, immediate replacement of runners stopped by a host reboot, and removal of root-owned job workspaces.

The entrypoint smoke test uses the production image with stubbed config/run scripts and networking disabled. It checks non-root execution, short-lived token consumption, ephemeral/update flags and completion hook permissions.

The Docker socket test runs the production entrypoint the way `--docker-socket` pools do: host socket, host network and a same-path workspace under `$HOME`. It checks that the non-root runner can use the daemon and that a container it starts reads and writes the runner's workspace. Run these checks on each Docker runtime before relying on it; see [platforms](../docs/platforms.md).

The responsiveness tests delay Docker responses and leave a socket client incomplete, then verify that status and stop remain responsive and changed intent prevents stale provisioning. App tests verify RSA signatures, installation token exchange, caching and refresh against a local API server. Test keys in `fixtures/` are public test data.

## Real GitHub compatibility suite

Use a dedicated test repository and a separate `--home` so production pools are unaffected. Install `tests/workflows/compatibility.yml` as `.github/workflows/compatibility.yml` in that repository's default branch. Authenticate using a profile with permissions for the intended organization or repository scope; see [authentication](../docs/authentication.md).

```bash
# After initializing and authenticating this separate home:
runnerctl --home /absolute/test-home pool add compatibility \
  --repo OWNER/TEST-REPO --auth test --labels UNIQUE-TEST-LABEL \
  --replicas 2 --image local/runnerctl-test-base:1 \
  --docker-socket --cpus 1 --memory 1g
runnerctl --home /absolute/test-home start compatibility
runnerctl --home /absolute/test-home manager
# In a second terminal:
gh workflow run compatibility.yml --repo OWNER/TEST-REPO -f runner_label=UNIQUE-TEST-LABEL
```

For organization runners, replace `--repo OWNER/TEST-REPO` with `--org OWNER` and allow that repository in the runner group's access policy. Never reuse a production label. The suite checks checkout and non-root shell execution, Docker Buildx and workspace binds, service access via localhost, and container jobs with JavaScript actions and service DNS.

Wait for all four jobs to complete before stopping the test pool. Idle ephemeral runners need another job to finish a graceful drain; after the test run is complete, `stop compatibility --force --wait` cleans up the dedicated idle runners. Verify both GitHub registrations and local containers disappeared, then stop the separate manager. See the [compatibility record](../docs/compatibility.md) for validated environments and remaining limits.
