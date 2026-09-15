#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd -P)"
cd "$repo_root"
if [[ "$(uname -s)" == Darwin ]]; then
  instance="${EXO_FIRECRACKER_LIMA_INSTANCE:-exo-firecracker}"
  bridge_target="${EXO_EGRESS_BRIDGE_TARGET_DIR:-/var/tmp/exo-egress-target}"
  bridge_binary="/usr/local/libexec/exo-egress-test-bridge-$$"
  limactl shell "$instance" -- bash -c '
    set -euo pipefail
    cd "$1"
    CARGO_TARGET_DIR="$2" CARGO_BUILD_JOBS=2 cargo build -p exoharness --example firecracker-bridge --features firecracker
    sudo -n install -o root -g root -m 755 "$2/debug/examples/firecracker-bridge" "$3"
  ' bash "$repo_root" "$bridge_target" "$bridge_binary"
  trap 'limactl shell "$instance" -- sudo -n rm -f "$bridge_binary"' EXIT
  EXO_EGRESS_BRIDGE_BINARY="$bridge_binary" \
    EXO_FIRECRACKER_LIMA_INSTANCE="$instance" \
    cargo test -p exoharness --features firecracker --lib \
      egress::tests::managed_firecracker_egress_live -- --exact --ignored --nocapture
  exit
fi
exec bash "$repo_root/support/firecracker/egress-smoke.sh" \
  egress::tests::managed_firecracker_egress_live
