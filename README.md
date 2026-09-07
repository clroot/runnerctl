# runnerctl

A Rust CLI for managing multiple GitHub Actions runner pools on a single Linux Docker host. Configure each pool's organization, authentication profile, labels, image, and capacity. The manager runs the official GitHub runner in ephemeral containers, removes containers after their jobs finish, and creates replacements.

## Requirements and installation

Requires Linux, Rust 1.85 or later, Docker Engine and CLI, and access to the Docker socket. A systemd user service is optional.

```bash
cargo install --path . --locked
# Build the image on the Docker host used by runnerctl.
docker build -t local/runnerctl-runner:2.337.0 runner
```

The image build verifies the runner archive's SHA-256 checksum. To build another version, supply both the `RUNNER_VERSION` and architecture-specific `RUNNER_SHA256` build arguments, and use a matching image tag. Custom images must follow the image contract below.

## Quick start

```bash
runnerctl init --org '<YOUR_ORG>'
runnerctl auth login --profile '<YOUR_ORG>'

runnerctl pool add validate --org '<YOUR_ORG>' --auth '<YOUR_ORG>' \
  --labels validate --replicas 4
runnerctl pool add build --org '<YOUR_ORG>' --auth '<YOUR_ORG>' \
  --labels build --replicas 2 --docker-socket

runnerctl doctor
runnerctl service install
runnerctl start --all
runnerctl status
```

PAT input is hidden at the terminal. For automation, use `auth login --profile NAME --token-stdin` or `--env VARIABLE`. Do not pass tokens as command arguments. Configuration stores only credential file or environment variable references. Credential files use mode `0600`, management directories use `0700`, and runner containers receive only short-lived registration tokens.

A fine-grained PAT needs the target organization's **Self-hosted runners: Read and write** permission and any approval required by organization policy. `doctor` checks runner read access and local image availability; registration write access is checked when creating a runner. GitHub App authentication is planned for a later release.

To run without a service, use `runnerctl manager` in the foreground. The CLI and manager must use the same `--home` directory. The default is `$XDG_CONFIG_HOME/runnerctl` or `~/.config/runnerctl`. When the manager is stopped, CLI configuration and intent changes are still saved under an exclusive lock, but container provisioning requires the manager to be running.

File credentials are recommended for systemd because user services do not automatically inherit shell environment variables. To keep the service running without an active login, configure `sudo loginctl enable-linger "$USER"`. `service install` creates a unit pointing to the current binary and enables and starts it. Reinstall the service if you move the binary.

## Managing multiple pools

```bash
runnerctl pool list
runnerctl pool show build
runnerctl status --pool build
runnerctl --json status
runnerctl scale build 3
runnerctl stop build
runnerctl start build
runnerctl logs <runner-id> --pool build --follow
runnerctl upgrade --pool build --image local/runnerctl-runner:2.337.0
runnerctl pool remove build
```

`replicas` is the pool's concurrent capacity, including busy runners. New pools are **stopped by default**. `scale` updates the desired capacity without starting a stopped pool. Starting or stopping every pool requires `--all`.

Stopping, scaling down, deleting, and updating pools limit new provisioning and wait for existing jobs to finish naturally. **Idle runners may remain until they receive and complete their next job.** Checking GitHub's busy flag cannot eliminate the race between job assignment and container termination, so an idle observation alone does not trigger forced termination. These runners are marked `draining`; elapsed time does not automatically cancel work. Use `stop build --force` when immediate termination is necessary; this can interrupt running jobs. After a forced stop, remote registration cleanup may remain pending until GitHub connectivity is restored.

Stopping the manager process does not stop running containers. On restart, the manager recovers from SQLite and container ownership labels. Recreating a pool with the same name assigns a new UUID, distinguishing it from the previous pool. Containers belonging to another manager or an existing Compose deployment are left untouched.

To route jobs to a specific pool within an organization, include its automatically registered pool label:

```yaml
jobs:
  validate:
    runs-on: [self-hosted, 'pool:validate', validate]
    steps:
      - uses: actions/checkout@v4
      - run: echo 'validation job'
```

Pool names are not access-control boundaries. Configure runner group repository access policies in GitHub. Sharing the host Docker socket is intended for trusted workflows and does not provide security isolation between pools. The default image includes the Docker CLI for Docker builds in shell steps. Workflows using `jobs.<job>.container`, service containers, or host bind mounts need separate validation of their workspace path mappings.

## Configuration

See [config.example.toml](config.example.toml) for three pools across two organizations.

```bash
runnerctl config check
runnerctl config apply
# Use optimistic concurrency control in external automation.
runnerctl config apply --revision 3
```

The sum of all configured replicas, including stopped pools, must not exceed `manager.max_runners`. Creation reservations and containers awaiting cleanup also count toward actual capacity. The current implementation rotates through pools and creates **one runner at a time**, staying within `max_parallel_creates`. Increasing that setting does not yet enable parallel creation.

Manual file edits take effect only after `config apply`. CLI configuration changes are rejected while unapplied edits exist, preventing accidental overwrites. Delete pools with `pool remove`. Changing a pool's organization or authentication profile requires the pool to be stopped and its previous registrations fully cleaned up. Image, label, and runner group changes apply to a new generation of runners.

SQLite is the source of truth for applied configuration. If the process exits after saving the database but before writing TOML, the file may differ from the applied state. Compare the applied configuration and revision from `--json status` with the file, then recover by applying the desired file contents with `config apply`. Saved credential files are not automatically deleted. After rotating a token, check that an old file is no longer referenced before removing it.

## Failures and logs

- GitHub and Docker are polled periodically. Docker event streaming is not yet implemented.
- API observation failures are reported as `unknown` and do not cause running containers to be deleted.
- Authentication errors and rate limits are shared within an authentication profile. Failed pools retry with exponential backoff and jitter.
- A runner that exits without completing a job is recorded as a failure. Normal completion requires both the completion hook and container exit.
- Replacement containers receive new names and fresh filesystems, avoiding registration state left by container restarts.
- A live but unresponsive process may remain `unknown`. Inspect its state and use an explicit `stop --force` followed by `start` if necessary.
- Before deleting a container, the manager saves recent logs, up to 1 MiB, and a `_diag` archive, up to 32 MiB, in `<home>/logs`. Archive failure delays deletion and reports an error.
- Docker logs are limited to three 10 MiB files. Archived files expire after seven days by default; retention cleanup runs during successful reconciliation of an active pool.
- `logs --follow` polls the selected container and ends when that container is replaced. Previous logs remain in the archive directory.
- Rolling updates replace runners after their jobs finish, without adding extra capacity. Startup failures trigger backoff for the affected pool while existing jobs continue.

Automatic runner updates are disabled; updates are delivered through images. Automatic tracking of runner version support deadlines is not yet implemented. Monitor GitHub's update requirements and rebuild images accordingly.

## Image contract

The default Dockerfile carries forward the Ubuntu, Git, and Docker CLI setup from the existing deployment. `/entrypoint.sh` starts as root to configure Docker socket group access, consumes `/run/runnerctl/token`, and then runs `config.sh --ephemeral --disableupdate` and `run.sh` as a regular user. The PAT is not injected into this container.

Custom images must handle `GITHUB_ORG`, `RUNNER_NAME`, `RUNNER_LABELS`, and `RUNNER_GROUP`, consume and remove `/run/runnerctl/token` during registration, and exit after one job. The completion hook must create `/run/runnerctl/completed`, and diagnostic files must be written to `/home/runner/actions-runner/_diag`. Extend `runner/Dockerfile` with additional build tools when possible.

## Testing and migration

```bash
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
```

See [tests/README.md](tests/README.md) for isolated tests using real Docker containers. These tests create and clean up only dedicated containers labeled with a randomly generated manager ID. They do not register runners with a real organization or execute GitHub workflows.

Start migration with one test runner using a new label. Verify your actual workflows and Docker usage, then stop existing Compose slots individually and increase the new pool's capacity. Adjust the existing recovery timer's management scope so it cannot recreate migrated runners, and disable it once migration is complete.

## References

- [GitHub runner API and permissions](https://docs.github.com/en/rest/actions/self-hosted-runners)
- [Ephemeral runners and update policies](https://docs.github.com/en/actions/reference/runners/self-hosted-runners)
- [Job completion hooks](https://docs.github.com/en/actions/how-tos/manage-runners/self-hosted-runners/run-scripts)
- [Implementation plan (Korean)](PLAN.md)
