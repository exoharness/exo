# exo-policy sandbox image: the container an Exoclaw *executor* (EE) runs inside
# when running in the split topology (EE in a kernel-owned sandbox, durable
# state in a remote kernel reached over HTTP). It bundles node + tsx, the TS
# policy/source, and a prebuilt exo binary so EE can run the harness in-place.
#
# Base is ubuntu:24.04 to match the glibc the host-built debug binary links
# against (a debian/bookworm node image would fail with a GLIBC version error).
#
# Build (from the repo root, with target/debug/exo already built):
#   docker build -t exo-policy:dev -f examples/exoclaw/policy-sandbox/Dockerfile .
# A .dockerignore excluding target/ (except target/debug/exo), .exo, .git is
# strongly recommended so the build context stays small.
#
# The code is placed at /home/worker/exo so the stored agent module path
# (an absolute host path) resolves identically inside the container.
FROM ubuntu:24.04
ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends nodejs npm ca-certificates \
    && npm i -g pnpm@10.26.2 \
    && apt-get clean && rm -rf /var/lib/apt/lists/*
WORKDIR /home/worker/exo
COPY package.json pnpm-lock.yaml ./
# --ignore-scripts skips native module builds (sodium-native, opusscript,
# @discordjs/voice); the harness does not import them at load time.
RUN pnpm install --ignore-scripts
COPY tsconfig.json ./
COPY typescript ./typescript
COPY examples ./examples
COPY target/debug/exo ./target/debug/exo
