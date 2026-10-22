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
docker run --rm --init --network host --security-opt no-new-privileges:true \
  -e GITHUB_ORG=test-org -e RUNNER_NAME=socket-test -e RUNNER_LABELS=test \
  -e RUNNER_ROOT="$root/runtime" -e RUNNER_UID="$(id -u)" -e SIBLING_IMAGE="$image" \
  -v "$root:/run/runnerctl" -v "$root/runtime:$root/runtime" \
  -v /var/run/docker.sock:/var/run/docker.sock \
  --entrypoint /bin/bash "$image" -c '
set -euo pipefail
cat > /home/runner/actions-runner/config.sh <<"SCRIPT"
#!/usr/bin/env bash
set -euo pipefail
test "$(id -u)" != 0
test ! -e /run/runnerctl/token
SCRIPT
cat > /home/runner/actions-runner/run.sh <<"SCRIPT"
#!/usr/bin/env bash
set -euo pipefail
test "$(id -u)" != 0
test "$PWD" = "$RUNNER_ROOT"
docker version >/dev/null
mkdir -p _work/job
echo from-runner > _work/job/input
docker run --rm --network none --entrypoint /bin/sh -v "$RUNNER_ROOT/_work/job:/job" \
  "$SIBLING_IMAGE" -c "test \"\$(cat /job/input)\" = from-runner && echo from-sibling > /job/output"
test "$(cat _work/job/output)" = from-sibling
bash "$ACTIONS_RUNNER_HOOK_JOB_COMPLETED"
printf "%s\n" "Docker socket: non-root daemon access and same-path workspace binds passed"
SCRIPT
exec /entrypoint.sh
'
test -f "$root/completed"
