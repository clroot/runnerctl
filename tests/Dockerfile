ARG RUNNER_IMAGE=local/runnerctl-test-base:1
FROM ${RUNNER_IMAGE}
COPY tests/stub-entrypoint.sh /stub-entrypoint.sh
ENTRYPOINT ["/bin/bash", "/stub-entrypoint.sh"]
