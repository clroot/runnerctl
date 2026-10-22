# runnerctl

[![crates.io](https://img.shields.io/crates/v/runnerctl.svg)](https://crates.io/crates/runnerctl)
[![CI](https://github.com/clroot/runnerctl/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/clroot/runnerctl/actions/workflows/ci.yml)
[![Rust](https://img.shields.io/badge/Rust-1.85%2B-orange.svg)](https://www.rust-lang.org)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

**Run several GitHub Actions runner pools on one Docker host. No Kubernetes. A fresh container for every job.**

For developers and small teams who already have a Linux server, a Mac, or a Windows PC with WSL2 and want to manage runner capacity, images, labels, and credentials from one CLI. Pools can belong to different GitHub organizations or individual repositories. Runner containers receive short-lived registration tokens; PATs and GitHub App private keys stay with the manager.

Example status:

```text
POOL                 SCOPE              INTENT     TARGET ACTIVE   IDLE   BUSY STARTING  UNKNOWN DRAINING
validate             example-team       running         2      2      1      1        0        0        0
build                octocat/project    running         1      1      0      1        0        0        0
```

The manager restores state after a restart, replaces completed runners, and preserves running jobs during normal stopping and upgrades. Capacity is configured per pool. This is a single-host manager for trusted workflows; pool labels and shared Docker access do not isolate tenants.

## Install with Cargo

On the Docker host, install Rust/Cargo 1.85 or later and a C compiler/linker. Docker must be running, and your user must be able to access its local socket. On a Mac or Windows PC, read [macOS and Windows](docs/platforms.md) first.

```bash
cargo install runnerctl --locked
runnerctl --version
runnerctl image pull
```

The CLI comes from crates.io. `image pull` downloads the matching versioned runner image from GHCR; releases build images for Linux amd64 and arm64. Add `~/.cargo/bin` to `PATH` if needed. Repeat `cargo install runnerctl --locked` to update the CLI.

## Your first runner

Create one organization pool with one runner:

```bash
runnerctl init --org YOUR_ORG
runnerctl auth login --profile YOUR_ORG
runnerctl pool add ci --org YOUR_ORG --auth YOUR_ORG --labels ci --replicas 1
runnerctl doctor
runnerctl service install
runnerctl start ci
runnerctl status --pool ci
```

`auth login` reads a PAT without echoing it. A fine-grained organization PAT needs **Self-hosted runners: Read and write**, plus any organization approval. See [authentication](docs/authentication.md) for GitHub Apps and automation.

`service install` starts a systemd user service on Linux and WSL2, or a LaunchAgent on macOS. On Linux, keep it running after logout by enabling lingering with `sudo loginctl enable-linger "$USER"`. You can also run `runnerctl manager` in another terminal. Commands and the manager must use the same `--home` directory.

Add this workflow to a repository accessible to the organization runner, then run it from GitHub Actions:

```yaml
name: runnerctl smoke test
on: workflow_dispatch
jobs:
  smoke:
    runs-on: [self-hosted, 'pool:ci', ci]
    steps:
      - uses: actions/checkout@v4
      - run: echo 'runnerctl is ready'
```

After the job, `status` shows a replacement with a new runner ID.

For an individual repository, use `init --repo OWNER/REPO` and `pool add ci --repo OWNER/REPO --auth OWNER --labels ci`. A fine-grained repository PAT needs **Administration: Read and write** for that repository. Organization and repository pools can coexist.

## Daily commands

```bash
runnerctl pool list
runnerctl status --pool ci
runnerctl --json status
runnerctl scale ci 3
runnerctl stop ci --wait --timeout 5m
runnerctl start ci
runnerctl logs RUNNER_ID --pool ci --follow
runnerctl pool remove ci --wait --timeout 5m
```

**Normal stop drains runners.** Replacements stop immediately, busy jobs finish, and idle runners can accept one final job before removal. `--wait` waits for cleanup; its timeout never escalates to cancellation. Use `--force` when you explicitly want to terminate existing work. Status explains whether draining runners are waiting for a job, finishing a job, or awaiting cleanup.

To enable Docker builds, job containers, and services, create a pool with `--docker-socket`. The provided image shares the host network and maps its runtime/workspace to matching host paths. This requires trusted workflows. Check the [workflow compatibility matrix](docs/compatibility.md).

```bash
runnerctl pool add build --org YOUR_ORG --auth YOUR_ORG --labels build \
  --replicas 2 --docker-socket --cpus 2 --memory 2g
```

CPU and memory limits apply to each runner container. Sibling job/service containers and Docker builds use the host daemon and need their own resource limits.

## Documentation

- [Authentication: PATs and GitHub Apps](docs/authentication.md)
- [macOS and Windows (WSL2)](docs/platforms.md)
- [Operations: configuration, draining, recovery, updates, and custom images](docs/operations.md)
- [Workflow compatibility and real GitHub checks](docs/compatibility.md)
- [Example configuration](config.example.toml)
- [Tests](tests/README.md) and [release procedure](docs/releases.md)

## Development

```bash
git clone https://github.com/clroot/runnerctl.git
cd runnerctl
cargo install --path . --locked
docker build -t local/runnerctl-test-base:1 runner
# Use --image local/runnerctl-test-base:1 when adding a development pool.
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
```

For migration, start with a single runner and a unique label, verify your workflows, then move capacity from your previous setup. Existing v0.1 organization/PAT configurations and saved state remain readable. Existing pool image references are preserved; choose the new image explicitly when upgrading a pool.

Licensed under [MIT](LICENSE). Contributions and reproducible bug reports are welcome; see [CONTRIBUTING.md](CONTRIBUTING.md).
