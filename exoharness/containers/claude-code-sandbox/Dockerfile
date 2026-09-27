FROM node:22-bookworm-slim

COPY version /tmp/claude-agent-sdk-version

ENV CLAUDE_CONFIG_DIR=/home/exo/.claude
ENV HOME=/home/exo
ENV PATH=/usr/local/bin:/usr/local/sbin:/usr/sbin:/usr/bin:/sbin:/bin

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        bash \
        ca-certificates \
        curl \
        git \
        openssh-client \
        ripgrep \
    && rm -rf /var/lib/apt/lists/*

RUN set -eu; \
    case "$(dpkg --print-architecture)" in \
        amd64) native_arch=x64 ;; \
        arm64) native_arch=arm64 ;; \
        *) echo "Unsupported Claude Agent SDK architecture" >&2; exit 1 ;; \
    esac; \
    version="$(cat /tmp/claude-agent-sdk-version)"; \
    native_package="@anthropic-ai/claude-agent-sdk-linux-${native_arch}"; \
    npm install -g \
        "@anthropic-ai/claude-agent-sdk@${version}" \
        "${native_package}@${version}"; \
    claude_bin="$(npm root -g)/${native_package}/claude"; \
    test -f "${claude_bin}"; \
    chmod +x "${claude_bin}"; \
    ln -sf "${claude_bin}" /usr/local/bin/claude-code; \
    rm /tmp/claude-agent-sdk-version

RUN mkdir -p /home/exo/.claude /home/exo/workspace

WORKDIR /home/exo/workspace

CMD ["bash"]
