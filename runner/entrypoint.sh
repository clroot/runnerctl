#!/usr/bin/env bash
set -euo pipefail
cd /home/runner/actions-runner
: "${GITHUB_ORG:?}" "${RUNNER_NAME:?}" "${RUNNER_LABELS:?}"
if [[ -S /var/run/docker.sock ]]; then
  socket_gid="$(stat -c '%g' /var/run/docker.sock)"
  group_name="$(getent group "$socket_gid" | cut -d: -f1 || true)"
  if [[ -z "$group_name" ]]; then group_name=hostdocker; groupadd -g "$socket_gid" "$group_name"; fi
  usermod -aG "$group_name" runner
fi
# The manager never reuses this container. A token is consumed only at initial boot.
token="$(cat /run/runnerctl/token)"
rm -f /run/runnerctl/token
chmod 733 /run/runnerctl
# Do not echo config.sh arguments or expose the token in Docker environment metadata.
gosu runner ./config.sh --unattended --ephemeral --disableupdate \
  --url "https://github.com/${GITHUB_ORG}" --token "$token" \
  --name "$RUNNER_NAME" --labels "$RUNNER_LABELS" --runnergroup "${RUNNER_GROUP:-Default}"
unset token
export ACTIONS_RUNNER_HOOK_JOB_COMPLETED=/usr/local/bin/runnerctl-job-completed.sh
exec gosu runner ./run.sh
