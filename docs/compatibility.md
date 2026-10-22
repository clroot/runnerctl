# Workflow compatibility

The bundled image is a small Ubuntu runner environment, not a copy of the GitHub-hosted tool cache. Install additional language toolchains in a derived image or use setup actions. Runners are Linux containers. The manager runs on Linux, on macOS, and on Windows in Command Prompt, PowerShell or WSL2, and needs a Docker daemon that can bind-mount its home directory; see [platforms](platforms.md).

## Validation record

On 2026-10-12, runnerctl 0.2.0 with Actions runner 2.338.0 was exercised on Linux amd64 with Docker Engine 29.9.0. Tests used dedicated pools with unique labels, two replicas, and per-runner limits of one CPU and 1 GiB memory.

| Check | Organization runner | Repository runner |
| --- | --- | --- |
| Checkout, shell and non-root execution | Passed | Passed |
| Buildx build, image execution and workspace bind | Passed | Passed |
| PostgreSQL service reached from shell job | Passed | Passed |
| Container job, checkout JavaScript runtime and service DNS | Passed | Passed |

On 2026-10-22, the local checks passed on macOS 26.6 (Apple silicon, arm64) with OrbStack 2.2.4 and Docker Engine 29.8.2: Docker lifecycle and recovery tests, the entrypoint smoke test and the Docker socket test. A host-network container also reached a service through a random published port, as the services job does. The real GitHub suite has not run on macOS.

| Host | Docker | Local checks | Real GitHub suite |
| --- | --- | --- | --- |
| Linux amd64 | Docker Engine | Passed in CI | Passed (2026-10-12) |
| macOS arm64 | OrbStack 2.2.4 | Passed (2026-10-22) | Not run |
| macOS | Docker Desktop | Not run | Not run |
| Windows amd64, WSL2 | Docker Desktop 4.95.0, host networking | Passed (2026-10-22) | Passed (2026-10-22) |
| Windows amd64, native | Docker Desktop 4.95.0, host networking | Passed (2026-10-22) | Passed (2026-10-22) |

On 2026-10-22, Windows 11 (build 26300, amd64) with Docker Desktop 4.95.0 (Docker Engine 29.8.2, WSL 2 backend) ran both Windows setups with Actions runner 2.338.0. In a Fedora WSL2 distribution, the Docker lifecycle and recovery tests, the entrypoint smoke test and the Docker socket test passed. With runnerctl built for Windows, the unit tests, including the access control checks, the Docker lifecycle and recovery tests and `tests\docker-socket.ps1` passed. A PowerShell and Command Prompt session ran the CLI against the manager's named pipe, `doctor`, `service install`, reinstall and `service uninstall`.

The real GitHub suite ran against a repository pool with two Docker socket runners for each setup. With Docker Desktop's default settings, checkout, shell and non-root execution, Buildx with workspace binds, and the container job with service DNS passed, but the shell job could not reach its PostgreSQL service through the random published port on `127.0.0.1`: with host networking off, Docker Desktop publishes ports only on the Windows side, out of reach of a host-network container in its VM. After host networking was enabled in Docker Desktop (Settings > Resources > Network), all four jobs passed in the [WSL2 run](https://github.com/eduvil-dev/runnerctl-validation/actions/runs/39766033162) and the [native run](https://github.com/eduvil-dev/runnerctl-validation/actions/runs/39766505881). Forced cleanup removed every container and registration in both setups.

The private test repository records the [organization run](https://github.com/eduvil-dev/runnerctl-validation/actions/runs/38200291103) and [repository run](https://github.com/eduvil-dev/runnerctl-validation/actions/runs/38200758703). Access requires repository permission. The reusable fixture is [tests/workflows/compatibility.yml](../tests/workflows/compatibility.yml), with reproduction steps in [tests/README.md](../tests/README.md).

Local regression tests also cover runner replacement, persisted intent and restart recovery, provisioning backoff, root-owned workspace cleanup, slow Docker responses and concurrent Unix socket clients. A live idle-pool check confirmed that `stop --wait --timeout 1s` leaves runners draining without cancelling them; an explicit `--force --wait` completed container and registration cleanup.

GitHub App JWT signing, installation token exchange, reuse and expiry refresh are tested against a local API server. Authentication against a real installed GitHub App has not been validated.

## Boundaries

- Docker builds, service containers and container jobs require `docker_socket = true`. These pools share the host network and Docker daemon and should run trusted workflows.
- CPU/memory settings constrain the runner container. Apply separate workflow `options` to limit sibling job/service containers; build processes in the daemon are also outside the runner's container limits.
- The image publication workflow builds amd64 and arm64. ARM64 execution has passed only the local checks on macOS. Rootless/remote Docker, SELinux-enforcing hosts, and arbitrary custom images have not been validated by this suite.
- Job cancellation with `--force` can leave workflow-created containers on the shared daemon. Inspect those resources after forced interruption.
- Custom images must follow the runtime/workspace and completion-marker contract described in [operations](operations.md).
