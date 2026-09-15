#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd -P)"
test_name="${1:-egress::tests::firecracker_transparent_egress_live}"
if [[ "$(uname -s)" == Darwin ]]; then
  exec limactl shell "${EXO_FIRECRACKER_LIMA_INSTANCE:-exo-firecracker}" -- \
    bash "$repo_root/support/firecracker/egress-smoke.sh" "$test_name"
fi
if [[ "$(uname -s)" != Linux ]]; then
  echo "This smoke test requires Linux/KVM or macOS with Firecracker in Lima." >&2
  exit 1
fi

cd "$repo_root"
build_output="$(mktemp)"
trap 'rm -f "$build_output"' EXIT
CARGO_TARGET_DIR="${EXO_EGRESS_TARGET_DIR:-/var/tmp/exo-egress-target}" \
  CARGO_BUILD_JOBS=2 cargo test -p exoharness --features firecracker \
  --lib --no-run --message-format=json-render-diagnostics > "$build_output"
test_binary="$(python3 - "$build_output" <<'PY'
import json
import sys

with open(sys.argv[1]) as output:
    binaries = [
        message["executable"]
        for line in output
        if (message := json.loads(line)).get("reason") == "compiler-artifact"
        and message.get("executable")
        and message["target"]["name"] == "exoharness"
    ]
if len(binaries) != 1:
    raise RuntimeError("expected exactly one Exoharness test executable")
print(binaries[0])
PY
)"
sudo -n "$test_binary" --exact "$test_name" \
  --ignored --nocapture
