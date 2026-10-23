# Operations

## Configuration and capacity

The default home is `$XDG_CONFIG_HOME/runnerctl` or `~/.config/runnerctl`, and `%LOCALAPPDATA%\runnerctl` on Windows. It contains configuration, SQLite state, the manager socket (on Windows, a file naming its named pipe), credential references, per-run files, and archived diagnostics. The manager and CLI must use the same `--home`.

```bash
runnerctl config check
runnerctl config apply
runnerctl config apply --revision 3
```

Manual edits take effect after `config apply`. Other configuration edits are rejected while unapplied file changes exist. SQLite is authoritative: after an interrupted write, compare `--json status` (applied pool configuration and revision) against TOML, then explicitly apply the desired file. Use `pool remove` to delete pools.

`replicas` includes busy runners. New pools start stopped; scaling a stopped pool does not start it. The sum of configured replicas, including stopped pools, cannot exceed `max_runners`. Creating runners and pending cleanup count toward actual capacity. Provisioning is serialized; the legacy `max_parallel_creates` setting does not enable parallel creation.

Organization and repository pools can coexist. Set exactly one of `organization` or `repository = "OWNER/REPO"`. Repository pools use GitHub's repository runner endpoints and do not support custom runner groups. Changing scope or authentication profile requires the old pool to be stopped and completely cleaned up. Image, label, group, Docker access, CPU, or memory changes create a new runner generation.

`cpus = 2.0` and `memory = "2g"` set limits on each runner container. Memory accepts an integer byte count or b/k/m/g suffix (binary multiples); the minimum is 6 MiB. These limits do not constrain sibling containers or the host Docker daemon. Use workflow container/service `options` for their limits.

## Draining and waiting

| Command | New replacements | Existing runners |
| --- | --- | --- |
| `stop POOL` | Disabled | Finish their next/current job, then clean up |
| `scale POOL N` | Maintain N when running | Excess runners drain |
| `upgrade --pool POOL --image IMAGE` | Use the new image | Previous generation drains |
| `pool remove POOL` | Disabled | Drain and delete the pool after cleanup |
| `stop POOL --force` | Disabled | Explicitly terminate containers; jobs can be interrupted |

Idle runners can accept one more job during drain. A GitHub idle observation cannot eliminate the race with job assignment, so the manager does not force-kill on that observation. Status reports `waiting_for_next_job`, `job_in_progress`, or `awaiting_observation_or_cleanup` for draining runners.

`stop` and `pool remove` support `--wait --timeout 5m`. The timeout ends only the CLI wait, leaving the recorded intent intact. It never adds a forced stop. A running manager is required to make cleanup progress. `--force --wait` waits for both container and registration cleanup; a GitHub outage can delay the latter. Forced interruption of a workflow can leave containers created by that workflow on the shared daemon.

## Recovery and responsiveness

The manager is the sole writer of state. Status and local intent changes are serviced while Docker/GitHub operations are in flight. If intent changes during an operation, the manager finishes that operation and reconciles from the new state before taking the next action. Doctor and log reads run separately from reconciliation using a state snapshot.

Stopping the manager does not stop containers. Restart recovery uses SQLite and container ownership labels. Recreating a pool gives it a new UUID. Unrelated containers are never deleted. Docker event streaming is not implemented; GitHub observations are cached for the configured poll interval. `observed_at` in JSON status records the last reconciliation observation time, not a guarantee of current remote state.

Observation errors produce `unknown` instead of deleting a live container. Failed pools use bounded exponential backoff with jitter, up to 600 seconds. Unexpected exits pause provisioning while observation and cleanup continue. A reboot of the Docker host is distinguished from a runner failure, allowing immediate replacement of its stopped containers. On Docker Desktop, OrbStack and WSL2 the Docker host is a Linux VM that can restart without the computer rebooting, so the manager reads its boot time from a short-lived container of the runner image. Normal completion requires both the completion marker and container exit.

A live but unresponsive runner can remain unknown. Inspect diagnostics, then explicitly stop with `--force` and restart if needed. No timeout automatically cancels work.

## Logs

```bash
runnerctl logs RUNNER_ID --pool POOL --follow
runnerctl doctor
journalctl --user -u runnerctl.service -f   # Linux and WSL2 service
tail -f ~/.config/runnerctl/manager.log      # macOS service
Get-Content -Wait $env:LOCALAPPDATA\runnerctl\manager.log   # Windows service (PowerShell)
```

Before deleting a container, the manager archives up to 1 MiB of recent Docker logs and up to 32 MiB of runner diagnostics in `<home>/logs`. Archive failure delays deletion. Docker log rotation keeps three 10 MiB files. Archived files expire after seven days by default; cleanup runs during successful reconciliation of active pools. Follow mode stops when its selected container is replaced; older logs remain in the archive directory.

## Runner image updates

Automatic runner self-updates are disabled. `doctor` reads `io.runnerctl.runner.version` from each local image and checks GitHub's latest stable runner release. It warns about an available update and fails the version check when that release is already more than 30 days old. The displayed deadline is an upper bound based on the latest release: older missed releases or critical security updates can require action sooner. Unknown custom image versions or an unavailable release API produce an explicit warning, not a claim that the image is current.

GitHub's [update policy](https://docs.github.com/en/actions/reference/runners/self-hosted-runners#runner-software-updates-on-self-hosted-runners) requires regular updates and can stop assigning jobs to outdated runners.

```bash
cargo install runnerctl --locked
runnerctl image pull
# Use the version tag matching the newly installed CLI.
runnerctl upgrade --pool ci --image ghcr.io/clroot/runnerctl-runner:v0.3.0
runnerctl doctor
```

Existing pools retain their configured images when the CLI is updated. The repository checks upstream releases daily and opens an update PR with verified asset digests; maintainers still review, test, and publish a release. See [release maintenance](releases.md).

## Custom images and Docker workflows

Extend `runner/Dockerfile` or the published image to install additional toolchains. The base includes Ubuntu, Git, curl, jq, Docker CLI, Buildx, and Compose. It does not mirror the full GitHub-hosted tool cache.

Custom entrypoints must:

- Accept `GITHUB_URL`, `RUNNER_SCOPE` (`org` or `repo`), `RUNNER_NAME`, and `RUNNER_LABELS`. `GITHUB_ORG` is retained for existing organization images; apply `RUNNER_GROUP` only to organization registration.
- Consume and remove `/run/runnerctl/token`, register with `--ephemeral --disableupdate`, run as a regular user, and exit after one job.
- Create `/run/runnerctl/completed` from the job-completion hook, and expose diagnostics at `/home/runner/actions-runner/_diag`.
- Set `io.runnerctl.runner.version` to the installed upstream runner version for diagnostics.
- When `RUNNER_ROOT` is set, keep the runner runtime and workspace at that absolute path. The manager mounts the same path on the host and in the runner so sibling containers can access workspaces and JavaScript runtimes. The provided entrypoint copies the runtime into that per-run directory and accepts `RUNNER_UID` for cleanup ownership.
- Include `/bin/sh`, `chown`, and `chmod`. When a job leaves root-owned workspace files, the manager uses the same image in a temporary container to restore ownership before removing that run's directory. This helper has no network or Docker socket and mounts only the stopped runner's directory.

With `docker_socket = true`, the runner receives the host Docker socket and host network. This is for trusted workflows. Pool labels are routing hints; use GitHub runner groups/repository policies for access control. Shared Docker access allows control over the host and other pools.

The runtime copy uses additional disk per active runner and is removed during normal cleanup. The daemon must see the manager's home at the same path: a local Docker Engine, or Docker Desktop and OrbStack with that folder shared. On native Windows, runnerctl uses the path at which Docker Desktop's VM mounts the drive instead. `doctor` checks this with a bind mount. The runner starts shell steps from scripts under that path without quoting, so these pools need a `--home` path without spaces. Rootless/remote Docker, SELinux-enforcing hosts, and arbitrary custom images need separate validation; see [compatibility](compatibility.md).
