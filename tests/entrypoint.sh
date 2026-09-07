#!/usr/bin/env bash
set -euo pipefail
# No network or real token: exercise the production entrypoint with runner stubs.
docker run --rm --network none --security-opt no-new-privileges:true \
  -e GITHUB_ORG=test-org -e RUNNER_NAME=entrypoint-test \
  -e RUNNER_LABELS=test -e RUNNER_GROUP=Default \
  --entrypoint /bin/bash local/runnerctl-runner:2.337.0 -c '
set -euo pipefail
mkdir -p /run/runnerctl
printf "%s" "test-token" > /run/runnerctl/token
cat > /home/runner/actions-runner/config.sh <<"SCRIPT"
#!/usr/bin/env bash
set -euo pipefail
test "$(id -u)" != 0
test ! -e /run/runnerctl/token
[[ " $* " == *" --ephemeral "* ]]
[[ " $* " == *" --disableupdate "* ]]
[[ " $* " == *" --token test-token "* ]]
SCRIPT
cat > /home/runner/actions-runner/run.sh <<"SCRIPT"
#!/usr/bin/env bash
set -euo pipefail
test "$(id -u)" != 0
test -z "${token:-}"
bash "$ACTIONS_RUNNER_HOOK_JOB_COMPLETED"
test -f /run/runnerctl/completed
printf "%s\n" "Entrypoint: non-root execution, token consumption and completion hook passed"
SCRIPT
exec /entrypoint.sh
'
