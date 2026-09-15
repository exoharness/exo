#!/usr/bin/env bash
# Uses an already registered o3 model. This command makes paid model calls.
set -euo pipefail
experiment="$(cd "$(dirname "$0")" && pwd)"
repo="$(cd "$experiment/../.." && pwd)"
exo="${EXO_BIN:-$repo/target/debug/exo}"
cd "$repo"
root="${EXO_ROOT:-$repo/.exo}"
id="glia-vidur-$(date -u +%Y%m%dT%H%M%SZ)-$$"
run="$experiment/.work/discovery/$id"
mkdir -p "$run/workspace"
cp "$experiment/candidate.py" "$run/workspace/candidate.py"
cp "$experiment/TASK.md" "$run/workspace/TASK.md"
docker image inspect exo-glia-vidur:2666cb64 --format '{{.Id}}' > "$run/image-id.txt"
"$exo" --root "$root" --harness glia agent create "$id" --slug "$id" \
  --model o3 --provider docker --sandbox-image exo-glia-vidur:2666cb64 \
  --sandbox-scope conversation --networking disabled --tool-creation disabled \
  --max-tool-round-trips 49
"$exo" --root "$root" conversation create "$id" experiment --slug experiment
"$exo" --root "$root" conversation mount add "$id" experiment "$run/workspace" /workspace --rw
printf 'Discovery workspace: %s\n' "$run/workspace"
# Preserve the CLI failure if model/sandbox setup or the turn fails.
"$exo" --root "$root" conversation send "$id" experiment "$(cat "$experiment/TASK.md")" \
  2>&1 | tee "$run/agent.log"
printf '\nValidate the restored final candidate externally:\n'
printf '  %q candidate %q\n' "$experiment/run.sh" "$run/workspace/candidate.py"
