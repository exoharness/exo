# Root filesystem for live guest-hardening tests. Its workload user can become
# root with passwordless sudo, as in the Codex sandbox image.
FROM debian:bookworm-slim@sha256:734a72b3d6142a71993b4bc580ca01d8ca687f77a2c422d96a961d3376b179bc

RUN apt-get update \
    && apt-get install -y --no-install-recommends python3 sudo \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 10001 exo \
    && useradd --uid 10001 --gid exo --create-home --shell /bin/bash exo \
    && printf 'exo ALL=(ALL) NOPASSWD: ALL\n' > /etc/sudoers.d/exo \
    && chmod 0440 /etc/sudoers.d/exo
