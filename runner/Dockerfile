FROM ubuntu:24.04

ARG RUNNER_VERSION=2.337.0
ARG TARGETARCH
ARG RUNNER_SHA256

ENV DEBIAN_FRONTEND=noninteractive

RUN apt-get update && apt-get install -y --no-install-recommends \
    curl \
    ca-certificates \
    git \
    gosu \
    jq \
    procps \
    unzip \
    zip \
    && rm -rf /var/lib/apt/lists/*

# Docker CLI만 설치한다. 데몬은 호스트 소켓을 그대로 쓰므로 엔진(docker-ce,
# containerd)까지 넣을 필요가 없다 — 이미지가 수백 MB 가벼워지고,
# 디스크 압박으로 인한 기동 실패 가능성도 줄어든다.
RUN install -m 0755 -d /etc/apt/keyrings \
    && curl -fsSL https://download.docker.com/linux/ubuntu/gpg -o /etc/apt/keyrings/docker.asc \
    && chmod a+r /etc/apt/keyrings/docker.asc \
    && echo "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/docker.asc] https://download.docker.com/linux/ubuntu $(. /etc/os-release && echo "${VERSION_CODENAME}") stable" \
       > /etc/apt/sources.list.d/docker.list \
    && apt-get update \
    && apt-get install -y --no-install-recommends \
       docker-ce-cli \
       docker-buildx-plugin \
       docker-compose-plugin \
    && rm -rf /var/lib/apt/lists/*

# runner 전용 사용자
RUN groupadd -f docker \
    && useradd -m -s /bin/bash runner \
    && usermod -aG docker runner

WORKDIR /home/runner/actions-runner

RUN BUILD_ARCH="${TARGETARCH:-$(dpkg --print-architecture)}" \
    && case "${BUILD_ARCH}" in \
      amd64) RUNNER_ARCH=x64 ;; \
      arm64) RUNNER_ARCH=arm64 ;; \
      arm|armhf) RUNNER_ARCH=arm ;; \
      x64) RUNNER_ARCH=x64 ;; \
      *) echo "Unsupported TARGETARCH: ${BUILD_ARCH}" >&2; exit 1 ;; \
    esac \
    && curl -fsSL -o runner.tar.gz \
    "https://github.com/actions/runner/releases/download/v${RUNNER_VERSION}/actions-runner-linux-${RUNNER_ARCH}-${RUNNER_VERSION}.tar.gz" \
    && if [ -z "${RUNNER_SHA256}" ]; then \
      test "${RUNNER_VERSION}" = "2.337.0"; \
      case "${RUNNER_ARCH}" in \
        x64) RUNNER_SHA256=70920811a4f8ad4328818682bca5c6469c1c942fab52448868071d0063816613 ;; \
        arm64) RUNNER_SHA256=9b1dc70626422526e3c94767cf024896beb15da5342a3f4819bf2feac13e0393 ;; \
        arm) RUNNER_SHA256=3ea05289b7f7e1bc9a7bb35e2f738a903e0f757cb27ed32a3ef701404cf6d8e8 ;; \
      esac; \
    fi \
    && echo "${RUNNER_SHA256}  runner.tar.gz" | sha256sum --check --strict \
    && tar xzf runner.tar.gz \
    && rm runner.tar.gz \
    && ./bin/installdependencies.sh

# _work 디렉토리를 미리 생성하고 runner 소유로 설정
RUN mkdir -p /home/runner/actions-runner/_work /home/runner/actions-runner/_diag \
    && chown -R runner:runner /home/runner

COPY job-completed.sh /usr/local/bin/runnerctl-job-completed.sh
RUN chmod +x /usr/local/bin/runnerctl-job-completed.sh
COPY entrypoint.sh /entrypoint.sh
RUN chmod +x /entrypoint.sh

# root로 시작 → entrypoint에서 권한 보정 후 runner로 전환
ENTRYPOINT ["/entrypoint.sh"]
