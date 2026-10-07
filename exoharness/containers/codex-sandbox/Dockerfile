FROM node:22-bookworm-slim

COPY version /tmp/codex-version

ENV CODEX_HOME=/home/exo/.codex
ENV HOME=/home/exo
ENV PATH=/usr/local/bin:/usr/local/sbin:/usr/sbin:/usr/bin:/sbin:/bin

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        bash \
        ca-certificates \
        curl \
        gh \
        git \
        openssh-client \
        ripgrep \
        sudo \
    && rm -rf /var/lib/apt/lists/*

RUN npm install -g "@openai/codex@$(cat /tmp/codex-version)" && rm /tmp/codex-version

ARG DEVBOX_TOOLS=false
RUN if [ "$DEVBOX_TOOLS" = true ]; then \
        apt-get update \
        && apt-get install -y --no-install-recommends jq python3 python3-pip python3-venv python-is-python3 \
        && rm -rf /var/lib/apt/lists/* \
        && npm install -g pnpm@10.26.2 typescript@5.9.3 tsx@4.21.0; \
    fi

RUN groupadd --gid 10001 exo \
    && useradd --uid 10001 --gid exo --create-home --shell /bin/bash exo \
    && printf 'exo ALL=(ALL) NOPASSWD: ALL\n' > /etc/sudoers.d/exo \
    && chmod 0440 /etc/sudoers.d/exo \
    && mkdir -p /home/exo/.codex /home/exo/workspace \
    && chown -R exo:exo /home/exo

WORKDIR /home/exo/workspace


CMD ["bash"]
