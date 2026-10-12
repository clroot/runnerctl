#!/usr/bin/env bash
set -euo pipefail
mkdir -p /home/runner/actions-runner/_diag
echo test > /home/runner/actions-runner/_diag/test.log
rm -f /run/runnerctl/token
if [[ -n "${RUNNER_ROOT:-}" ]]; then
  # Model a sibling job container writing a root-owned workspace.
  mkdir -p "$RUNNER_ROOT/_work/root-owned"
  echo artifact > "$RUNNER_ROOT/_work/root-owned/result"
  chmod 700 "$RUNNER_ROOT/_work/root-owned"
fi
sleep 2
touch /run/runnerctl/completed
