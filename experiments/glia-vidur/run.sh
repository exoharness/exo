#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")"
mode="${1:?Usage: run.sh smoke|llq|lor|round_robin|hra|candidate [candidate.py]}"
args=()
case "$mode" in
  smoke) args=(--policy llq --smoke); cache=smoke ;;
  llq|lor|round_robin) args=(--policy "$mode"); cache=full ;;
  hra) candidate="$PWD/reference_hra.py"; args=(--policy candidate --candidate /candidate.py); cache=full ;;
  candidate) candidate="$(python3 -c 'from pathlib import Path; import sys; print(Path(sys.argv[1]).resolve(strict=True))' "${2:?candidate.py path required}")"; args=(--policy candidate --candidate /candidate.py); cache=full ;;
  *) echo "Unknown mode: $mode" >&2; exit 2 ;;
esac
run="$PWD/.work/runs/$(date -u +%Y%m%dT%H%M%SZ)-$mode-$$"
mkdir -p "$run" "$PWD/.work/cache/$cache"
docker image inspect exo-glia-vidur:2666cb64 --format '{{.Id}}' > "$run/image-id.txt"
printf 'Results: %s\n' "$run"
docker_args=(run --rm --network none --cpus 2 --memory 1536m
  --mount "type=bind,src=$run,dst=/workspace"
  --mount "type=bind,src=$PWD/.work/cache/$cache,dst=/cache")
if [[ -n "${candidate:-}" ]]; then
  docker_args+=(--mount "type=bind,src=$candidate,dst=/candidate.py,readonly")
fi
docker "${docker_args[@]}" exo-glia-vidur:2666cb64 \
  python /opt/experiment/benchmark.py "${args[@]}" \
  --seed "${VIDUR_SEED:-42}" --output /workspace/result --cache /cache \
  2>&1 | tee "$run/console.log"
