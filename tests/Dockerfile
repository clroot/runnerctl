FROM local/runnerctl-runner:2.337.0
ENTRYPOINT ["/bin/bash", "-c", "mkdir -p /home/runner/actions-runner/_diag; echo test > /home/runner/actions-runner/_diag/test.log; rm -f /run/runnerctl/token; sleep 2; touch /run/runnerctl/completed"]
