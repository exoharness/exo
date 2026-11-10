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
      https://github.com/smol-machines/smolvm/releases/download/v1.32.4/smolvm-1.32.4-linux-x86_64.tar.gz \
      --output "$archive"
    printf '7b0580c2ef98e183ac5c3a1567eca3a336d58aa0e2ca5904350f7d3a851205e8  %s\n' "$archive" | sha256sum --check --strict
    tar -xzf "$archive" -C "$artifact_dir"
    runtime_dir="$artifact_dir/smolvm-1.32.4-linux-x86_64"
    "$runtime_dir/smolvm" --version
    printf '%s\n' "$runtime_dir" >> "$GITHUB_PATH"
    printf 'SMOLVM_BIN=%s/smolvm\n' "$runtime_dir" >> "$GITHUB_ENV"
    ;;
  firecracker)
    # Pinned bundle documented in support/firecracker/README.md. Build the guest
    # from this checkout so the host/guest protocol versions always match.
    sudo apt-get update
    sudo apt-get install -y --no-install-recommends cpio e2fsprogs iproute2 iptables nftables xfsprogs
    sudo swapoff --all
    sudo modprobe vhost_vsock
    sudo sysctl -w net.ipv4.ip_forward=1
    archive="$artifact_dir/firecracker.tgz"
    curl --fail --location --retry 3 --max-time 120 \
      https://github.com/firecracker-microvm/firecracker/releases/download/v1.18.0/firecracker-v1.18.0-x86_64.tgz \
      --output "$archive"
    printf '1ac8eec866dd5eea14efecf6323e22b23eea24054545338a5b34bc5bca6623cd  %s\n' "$archive" | sha256sum --check --strict
    tar -xzf "$archive" -C "$artifact_dir"
    # Hosted runners allow user writes under /usr/local/bin. Firecracker's
    # trusted artifacts require root ownership all the way to the filesystem root.
    runtime_dir=/var/lib/exo/firecracker/bin
    sudo install -d -o root -g root -m 0755 "$runtime_dir"
    sudo install -o root -g root -m 0755 \
      "$artifact_dir/release-v1.18.0-x86_64/firecracker-v1.18.0-x86_64" "$runtime_dir/firecracker"
    sudo install -o root -g root -m 0755 \
      "$artifact_dir/release-v1.18.0-x86_64/jailer-v1.18.0-x86_64" "$runtime_dir/jailer"
    kernel="$artifact_dir/vmlinux"
    curl --fail --location --retry 3 --max-time 120 \
      https://s3.amazonaws.com/spec.ccfc.min/firecracker-ci/20261105-6ea6634994fa-0/x86_64/vmlinux-6.18.56 \
      --output "$kernel"
    printf 'af348226aa2a25b846156150d5cdb5dae3bf2a022f1db176e3a2b9a3249817bf  %s\n' "$kernel" | sha256sum --check --strict
    sudo install -d -o root -g root -m 0755 /var/lib/exo/firecracker
    sudo truncate -s 16G /var/lib/exo/firecracker/test-state.img
    sudo mkfs.xfs -f /var/lib/exo/firecracker/test-state.img
    sudo install -d -o root -g root -m 0700 /exo-test
    sudo mount -o loop /var/lib/exo/firecracker/test-state.img /exo-test
    sudo chmod 0700 /exo-test
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
