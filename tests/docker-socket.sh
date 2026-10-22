#!/usr/bin/env bash
set -euo pipefail
# No GitHub access: check the docker_socket contract on this Docker runtime. The non-root
# runner reaches the daemon through the mounted socket, and a sibling container sees the
# runner's workspace at the same host path, as container jobs and workspace binds require.
image="${RUNNER_IMAGE:-local/runnerctl-test-base:1}"
# Use the home directory like the default --home; Docker Desktop shares it with its VM.
root="$(mktemp -d "$HOME/.runnerctl-socket-test.XXXXXX")"
cleanup() {
  # Sibling containers can leave root-owned files on Linux hosts.
  docker run --rm --network none --user 0 --entrypoint /bin/sh -v "$root:/cleanup" "$image" \
    -c 'rm -rf /cleanup/runtime' >/dev/null 2>&1 || true
  rm -rf "$root"
}
trap cleanup EXIT
mkdir -p "$root/runtime"
chmod 700 "$root" "$root/runtime"
printf '%s' test-token > "$root/token"
chmod 600 "$root/token"
# Copy the container script into the shared directory: the repository may not be shared.
cp "$(dirname "$0")/docker-socket-runner.sh" "$root/socket-test.sh"
docker run --rm --init --network host --security-opt no-new-privileges:true \
  -e GITHUB_ORG=test-org -e RUNNER_NAME=socket-test -e RUNNER_LABELS=test \
  -e RUNNER_ROOT="$root/runtime" -e RUNNER_UID="$(id -u)" -e SIBLING_IMAGE="$image" \
  -v "$root:/run/runnerctl" -v "$root/runtime:$root/runtime" \
  -v "$root/socket-test.sh:/socket-test.sh:ro" \
  -v /var/run/docker.sock:/var/run/docker.sock \
  --entrypoint /bin/bash "$image" /socket-test.sh
test -f "$root/completed"
