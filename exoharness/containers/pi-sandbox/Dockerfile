FROM node:22-bookworm-slim

ARG PI_PACKAGE=@earendil-works/pi-coding-agent@0.85.1

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

# pi's bundle imports globSync from node:fs, so the base image must be node 22.
RUN npm install -g --ignore-scripts "${PI_PACKAGE}"

ARG DEVBOX_TOOLS=false
RUN if [ "$DEVBOX_TOOLS" = true ]; then \
        apt-get update \
        && apt-get install -y --no-install-recommends gh jq python3 python3-pip python3-venv python-is-python3 \
        && rm -rf /var/lib/apt/lists/* \
        && npm install -g pnpm@10.26.2 typescript@5.9.3 tsx@4.21.0; \
    fi

RUN mkdir -p /home/exo/.pi /home/exo/workspace

WORKDIR /home/exo/workspace

CMD ["bash"]
