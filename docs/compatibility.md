# Workflow compatibility

The bundled image is a small Ubuntu runner environment, not a copy of the GitHub-hosted tool cache. Install additional language toolchains in a derived image or use setup actions. Linux and a local Docker daemon are required.

## Validation record

On 2026-10-12, the upcoming runnerctl 0.2.0 with Actions runner 2.338.0 was exercised on Linux amd64 with Docker Engine 29.9.0. Tests used dedicated pools with unique labels, two replicas, and per-runner limits of one CPU and 1 GiB memory.

| Check | Organization runner | Repository runner |
| --- | --- | --- |
| Checkout, shell and non-root execution | Passed | Passed |
| Buildx build, image execution and workspace bind | Passed | Passed |
| PostgreSQL service reached from shell job | Passed | Passed |
| Container job, checkout JavaScript runtime and service DNS | Passed | Passed |

The private test repository records the [organization run](https://github.com/eduvil-dev/runnerctl-validation/actions/runs/38200291103) and [repository run](https://github.com/eduvil-dev/runnerctl-validation/actions/runs/38200758703). Access requires repository permission. The reusable fixture is [tests/workflows/compatibility.yml](../tests/workflows/compatibility.yml), with reproduction steps in [tests/README.md](../tests/README.md).

Local regression tests also cover runner replacement, persisted intent and restart recovery, provisioning backoff, root-owned workspace cleanup, slow Docker responses and concurrent Unix socket clients. A live idle-pool check confirmed that `stop --wait --timeout 1s` leaves runners draining without cancelling them; an explicit `--force --wait` completed container and registration cleanup.

GitHub App JWT signing, installation token exchange, reuse and expiry refresh are tested against a local API server. Authentication against a real installed GitHub App has not been validated.

## Boundaries

- Docker builds, service containers and container jobs require `docker_socket = true`. These pools share the host network and Docker daemon and should run trusted workflows.
- CPU/memory settings constrain the runner container. Apply separate workflow `options` to limit sibling job/service containers; build processes in the daemon are also outside the runner's container limits.
- The image publication workflow builds amd64 and arm64. ARM64 execution, rootless/remote Docker, SELinux-enforcing hosts, and arbitrary custom images have not been validated by this suite.
- Job cancellation with `--force` can leave workflow-created containers on the shared daemon. Inspect those resources after forced interruption.
- Custom images must follow the runtime/workspace and completion-marker contract described in [operations](operations.md).
