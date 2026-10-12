#!/usr/bin/env bash
set -euo pipefail
cd /home/runner/actions-runner
: "${RUNNER_NAME:?}" "${RUNNER_LABELS:?}"
github_url="${GITHUB_URL:-https://github.com/${GITHUB_ORG:?}}"
if [[ -S /var/run/docker.sock ]]; then
  socket_gid="$(stat -c '%g' /var/run/docker.sock)"
  group_name="$(getent group "$socket_gid" | cut -d: -f1 || true)"
  if [[ -z "$group_name" ]]; then group_name=hostdocker; groupadd -g "$socket_gid" "$group_name"; fi
  usermod -aG "$group_name" runner
fi
if [[ -n "${RUNNER_ROOT:-}" ]]; then
  if [[ "${RUNNER_UID:-0}" != 0 ]]; then usermod -o -u "$RUNNER_UID" runner; fi
  cp -a /home/runner/actions-runner/. "$RUNNER_ROOT/"
  chown -R runner:runner "$RUNNER_ROOT"
  # Retain the diagnostic image contract for docker cp and old tooling.
  rmdir /home/runner/actions-runner/_diag
  ln -s "$RUNNER_ROOT/_diag" /home/runner/actions-runner/_diag
  cd "$RUNNER_ROOT"
fi
# The manager never reuses this container. A token is consumed only at initial boot.
token="$(cat /run/runnerctl/token)"
rm -f /run/runnerctl/token
chmod 733 /run/runnerctl
# Do not echo config.sh arguments or expose the token in Docker environment metadata.
args=(--unattended --ephemeral --disableupdate --url "$github_url" --token "$token" \
  --name "$RUNNER_NAME" --labels "$RUNNER_LABELS")
if [[ "${RUNNER_SCOPE:-org}" == org ]]; then args+=(--runnergroup "${RUNNER_GROUP:-Default}"); fi
gosu runner ./config.sh "${args[@]}"
unset token
unset args
export ACTIONS_RUNNER_HOOK_JOB_COMPLETED=/usr/local/bin/runnerctl-job-completed.sh
exec gosu runner ./run.sh
