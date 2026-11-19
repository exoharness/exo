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
    # Pinned bundle from support/firecracker/bundle.env. Build the guest from
    # this checkout so the host/guest protocol versions always match.
    source support/firecracker/bundle.env
    sudo apt-get update
    sudo apt-get install -y --no-install-recommends cpio e2fsprogs iproute2 iptables nftables xfsprogs
    sudo swapoff --all
    sudo modprobe vhost_vsock
    sudo sysctl -w net.ipv4.ip_forward=1
    release="v${FIRECRACKER_VERSION}-x86_64"
    archive="$artifact_dir/firecracker.tgz"
    curl --fail --location --retry 3 --max-time 120 \
      "https://github.com/firecracker-microvm/firecracker/releases/download/v${FIRECRACKER_VERSION}/firecracker-${release}.tgz" \
      --output "$archive"
    printf '%s  %s\n' "$FIRECRACKER_X86_64_SHA256" "$archive" | sha256sum --check --strict
    tar -xzf "$archive" -C "$artifact_dir"
    # Hosted runners allow user writes under /usr/local/bin. Firecracker's
    # trusted artifacts require root ownership all the way to the filesystem root.
    runtime_dir=/var/lib/exo/firecracker/bin
    sudo install -d -o root -g root -m 0755 "$runtime_dir"
    sudo install -o root -g root -m 0755 \
      "$artifact_dir/release-${release}/firecracker-${release}" "$runtime_dir/firecracker"
    sudo install -o root -g root -m 0755 \
      "$artifact_dir/release-${release}/jailer-${release}" "$runtime_dir/jailer"
    kernel="$artifact_dir/vmlinux"
    curl --fail --location --retry 3 --max-time 120 \
      "https://s3.amazonaws.com/spec.ccfc.min/firecracker-ci/${FIRECRACKER_KERNEL_BUILD}/x86_64/vmlinux-${FIRECRACKER_KERNEL_VERSION}" \
      --output "$kernel"
    printf '%s  %s\n' "$FIRECRACKER_KERNEL_X86_64_SHA256" "$kernel" | sha256sum --check --strict
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
    # Docker only assembles the guest-root test filesystem; Exo boots the
    # resulting ext4 image. Extract as root to keep ownership and setuid bits.
    sudo_image=/var/lib/exo/firecracker/sudo-rootfs.ext4
    docker build --quiet --tag exo-ci-sudo-rootfs support/ci/sudo-rootfs
    sudo_container="$(docker create exo-ci-sudo-rootfs)"
    sudo install -d -m 0755 "$artifact_dir/sudo-rootfs"
    docker export "$sudo_container" |
      sudo tar -x --same-owner --preserve-permissions -C "$artifact_dir/sudo-rootfs"
    docker rm "$sudo_container"
    sudo truncate -s 1G "$sudo_image"
    sudo mkfs.ext4 -q -F -d "$artifact_dir/sudo-rootfs" "$sudo_image"
    "$runtime_dir/firecracker" --version
    "$runtime_dir/jailer" --version
    printf 'EXO_FIRECRACKER_BINARY=%s/firecracker\n' "$runtime_dir" >> "$GITHUB_ENV"
    printf 'EXO_FIRECRACKER_JAILER=%s/jailer\n' "$runtime_dir" >> "$GITHUB_ENV"
    printf 'EXO_TEST_FIRECRACKER_SUDO_IMAGE=%s\n' "$sudo_image" >> "$GITHUB_ENV"
    ;;
  *)
    echo "Unsupported CI sandbox backend: $sandbox_backend" >&2
    exit 1
    ;;
esac
