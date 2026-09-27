#!/usr/bin/env bash
set -euo pipefail

# Run on the disposable Linux x86_64 CI runner, after installing Rust.
[[ "$(uname -s)" == Linux && "$(uname -m)" == x86_64 ]]
sandbox_backend="${1:?expected smolvm or firecracker}"
artifact_dir="$(mktemp -d "${RUNNER_TEMP:?}/exo-sandbox.XXXXXX")"

case "$sandbox_backend" in
  smolvm)
    # Match the engine version used by the smolmachines dependency.
    archive="$artifact_dir/smolvm.tar.gz"
    curl --fail --location --retry 3 --max-time 120 \
      https://github.com/smol-machines/smolvm/releases/download/v1.19.0/smolvm-1.19.0-linux-x86_64.tar.gz \
      --output "$archive"
    printf '4feb274fd24d4722d718b9c85881fb9bba3c0c9f8123f67584436065bf583311  %s\n' "$archive" | sha256sum --check --strict
    tar -xzf "$archive" -C "$artifact_dir"
    runtime_dir="$artifact_dir/smolvm-1.19.0-linux-x86_64"
    "$runtime_dir/smolvm" --version
    printf '%s\n' "$runtime_dir" >> "$GITHUB_PATH"
    printf 'SMOLVM_BIN=%s/smolvm\n' "$runtime_dir" >> "$GITHUB_ENV"
    ;;
  firecracker)
    # Pinned bundle documented in support/firecracker/README.md. Build the guest
    # from this checkout so the host/guest protocol versions always match.
    sudo apt-get update
    sudo apt-get install -y --no-install-recommends cpio e2fsprogs iproute2 iptables nftables
    sudo modprobe vhost_vsock
    sudo sysctl -w net.ipv4.ip_forward=1
    archive="$artifact_dir/firecracker.tgz"
    curl --fail --location --retry 3 --max-time 120 \
      https://github.com/firecracker-microvm/firecracker/releases/download/v1.16.1/firecracker-v1.16.1-x86_64.tgz \
      --output "$archive"
    printf '382a02a869e4d6d5cb14c40577f9545e8458021ea8b0b2d3fc10ec14d9c242e6  %s\n' "$archive" | sha256sum --check --strict
    tar -xzf "$archive" -C "$artifact_dir"
    # Hosted runners allow user writes under /usr/local/bin. Firecracker's
    # trusted artifacts require root ownership all the way to the filesystem root.
    runtime_dir=/var/lib/exo/firecracker/bin
    sudo install -d -o root -g root -m 0755 "$runtime_dir"
    sudo install -o root -g root -m 0755 \
      "$artifact_dir/release-v1.16.1-x86_64/firecracker-v1.16.1-x86_64" "$runtime_dir/firecracker"
    sudo install -o root -g root -m 0755 \
      "$artifact_dir/release-v1.16.1-x86_64/jailer-v1.16.1-x86_64" "$runtime_dir/jailer"
    kernel="$artifact_dir/vmlinux"
    curl --fail --location --retry 3 --max-time 120 \
      https://s3.amazonaws.com/spec.ccfc.min/firecracker-ci/20260812-48f1b9fb52e9-0/x86_64/vmlinux-6.18.39 \
      --output "$kernel"
    printf '3fde810177aaf9d9a465d00e915924a7a6e2bdd34869d7fcd33c6af73b45e12d  %s\n' "$kernel" | sha256sum --check --strict
    sudo install -d -o root -g root -m 0755 /var/lib/exo/firecracker
    sudo install -o root -g root -m 0644 "$kernel" /var/lib/exo/firecracker/vmlinux
    guest_runtime="$(bash support/firecracker/build-guest.sh)"
    bash support/firecracker/build-initramfs.sh \
      --guest-runtime "$guest_runtime" --output "$artifact_dir/initramfs.cpio"
    sudo install -o root -g root -m 0644 \
      "$artifact_dir/initramfs.cpio" /var/lib/exo/firecracker/exo-firecracker-initramfs.cpio
    "$runtime_dir/firecracker" --version
    "$runtime_dir/jailer" --version
    printf 'EXO_FIRECRACKER_BINARY=%s/firecracker\n' "$runtime_dir" >> "$GITHUB_ENV"
    printf 'EXO_FIRECRACKER_JAILER=%s/jailer\n' "$runtime_dir" >> "$GITHUB_ENV"
    ;;
  *)
    echo "Unsupported CI sandbox backend: $sandbox_backend" >&2
    exit 1
    ;;
esac
