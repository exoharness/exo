#!/usr/bin/env bash
# One-time setup to evaluate exo on the Continual Learning Bench
# (continual-learning-bench.com / github.com/pgasawa/continual-learning-bench).
# Idempotent.
#
#   ./setup.sh
#
# Clones clbench, installs its env, symlinks our exo *system* into clbench's
# discovery path (src/systems/exo), and ensures a host exo binary exists.
# Prereqs you provide: uv, Docker, Python 3.13 (clbench needs it), and provider
# keys (OPENAI_API_KEY) in env / a clbench .env.
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
EXO_REPO="$(cd "$HERE/../.." && pwd)"
CLBENCH="${CLBENCH_REPO:-$(cd "$HERE/../../.." && pwd)/clbench}"

echo "==> clbench repo: $CLBENCH"
if [ ! -d "$CLBENCH/.git" ]; then
  git clone https://github.com/pgasawa/continual-learning-bench.git "$CLBENCH"
fi

echo "==> uv sync (clbench env)"
( cd "$CLBENCH" && uv sync --all-extras )

# Discovery is filesystem-based on src/systems/; symlink our system in as `exo`.
echo "==> linking exo system into clbench"
ln -sfn "$HERE/system" "$CLBENCH/src/systems/exo"
ls -ld "$CLBENCH/src/systems/exo"

# Discovery is filesystem-based on src/tasks/ too; symlink our custom task(s) in.
echo "==> linking custom tasks into clbench"
ln -sfn "$HERE/tasks/tool_forge" "$CLBENCH/src/tasks/tool_forge"
ls -ld "$CLBENCH/src/tasks/tool_forge"

echo "==> host exo binary (local-process sandbox; built from $EXO_REPO)"
[ -x "$EXO_REPO/target/release/exo" ] || ( cd "$EXO_REPO" && cargo build --release -p exo )

# Bring up assets/images for the smoke task (set SETUP=--all for everything).
echo "==> clbench setup (${SETUP:-exploitable_poker})"
( cd "$CLBENCH" && uv run clbench setup ${SETUP:-exploitable_poker} ) || \
  echo "    (setup step skipped/failed — run 'clbench setup --all' manually if needed)"

echo "==> Done. Verify: (cd $CLBENCH && uv run clbench list) ; then OPENAI_API_KEY=... $HERE/run.sh"
