FROM node:22-bookworm-slim

ARG CLAUDE_AGENT_SDK_PACKAGE=@anthropic-ai/claude-agent-sdk

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

# The Claude Code CLI ships as a per-platform native package next to the SDK.
# Pick the one for the image's own architecture so this file builds under
# Apple container (arm64) and Docker on x86 hosts alike.
RUN case "$(uname -m)" in \
        x86_64) arch=x64 ;; \
        aarch64) arch=arm64 ;; \
        *) echo "unsupported architecture $(uname -m)" >&2; exit 1 ;; \
    esac \
    && npm install -g \
        "${CLAUDE_AGENT_SDK_PACKAGE}" \
        "@anthropic-ai/claude-agent-sdk-linux-${arch}" \
    && CLAUDE_BIN="$(find "$(npm root -g)" -path "*/@anthropic-ai/claude-agent-sdk-linux-${arch}/claude" -type f -print -quit)" \
    && test -n "${CLAUDE_BIN}" \
    && chmod +x "${CLAUDE_BIN}" \
    && ln -sf "${CLAUDE_BIN}" /usr/local/bin/claude-code

RUN mkdir -p /home/exo/.claude /home/exo/workspace

WORKDIR /home/exo/workspace

CMD ["bash"]
