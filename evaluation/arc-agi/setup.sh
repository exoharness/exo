#!/usr/bin/env bash
# One-time setup to evaluate exo on ARC-AGI (arcprize.org). Idempotent.
#
#   ./setup.sh
#
# Clones the public ARC-AGI datasets (v1 + v2) as siblings of the exo repo and
# ensures a host exo binary exists. The datasets are just JSON task files — no
# framework to install (unlike harbor/clbench); arc_runner.py is the harness.
# Prereq you provide: OPENAI_API_KEY (for the actual run).
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
EXO_REPO="$(cd "$HERE/../.." && pwd)"
PARENT="$(cd "$HERE/../../.." && pwd)"

ARC_REPO_V1="${ARC_REPO_V1:-$PARENT/ARC-AGI}"
ARC_REPO_V2="${ARC_REPO_V2:-$PARENT/ARC-AGI-2}"

clone() {  # $1 = url, $2 = dest
  if [ ! -d "$2/.git" ]; then
    echo "==> cloning $1 -> $2"
    git clone --depth 1 "$1" "$2"
  else
    echo "==> present: $2"
  fi
}
clone https://github.com/fchollet/ARC-AGI.git        "$ARC_REPO_V1"
clone https://github.com/arcprize/ARC-AGI-2.git      "$ARC_REPO_V2"

echo "==> task counts"
for d in "$ARC_REPO_V1/data/evaluation" "$ARC_REPO_V1/data/training" \
         "$ARC_REPO_V2/data/evaluation" "$ARC_REPO_V2/data/training"; do
  [ -d "$d" ] && echo "    $(ls "$d"/*.json 2>/dev/null | wc -l)  $d"
done

echo "==> host exo binary"
if [ ! -x "$EXO_REPO/target/release/exo" ]; then
  echo "    building (cargo build --release -p exo)"
  ( cd "$EXO_REPO" && cargo build --release -p exo )
else
  echo "    present: $EXO_REPO/target/release/exo"
fi

echo "==> Done. Run: OPENAI_API_KEY=... ./run.sh           # 10 ARC-AGI-1 eval tasks"
echo "             OPENAI_API_KEY=... ARC_VERSION=2 ARC_N=20 ./run.sh"
