#!/usr/bin/env bash
# Runs inside the runner image for docker-socket.sh and docker-socket.ps1: stub config.sh
# and run.sh, then start the production entrypoint the way --docker-socket pools do.
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
