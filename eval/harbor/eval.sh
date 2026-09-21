#!/usr/bin/env bash
set -euo pipefail

eval_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(cd "$eval_dir/../.." && pwd)
venv="$eval_dir/.venv"

# uv rather than pip: litellm[proxy] declares rich<14 while harbor needs 14.1+,
# and only uv can override a dependency's constraint (see overrides.txt).
if ! command -v uv >/dev/null 2>&1; then
  echo "uv is required to set up the eval: https://docs.astral.sh/uv/getting-started/installation/" >&2
  exit 1
fi

if [[ ! -x "$venv/bin/python" ]]; then
  uv venv --quiet --python 3.12 "$venv"
fi

# backoff comes only from litellm[proxy], so its absence means a stale venv.
if ! "$venv/bin/python" -c 'import harbor, exo_harbor, backoff' 2>/dev/null; then
  uv pip install --quiet --python "$venv/bin/python" \
    --override "$eval_dir/overrides.txt" -e "$eval_dir"
fi

command=("$venv/bin/python" "$eval_dir/eval.py" "$@")

# Harbor runs each benchmark task in a Docker container. Most shells can use
# Docker directly; this workspace needs its configured docker group activated.
if docker info >/dev/null 2>&1; then
  cd "$repo_root"
  exec "${command[@]}"
fi

if command -v sg >/dev/null 2>&1 && getent group docker >/dev/null 2>&1; then
  printf -v quoted_command '%q ' "${command[@]}"
  cd "$repo_root"
  exec sg docker -c "$quoted_command"
fi

echo "Docker is unavailable. Start Docker or grant this user Docker access." >&2
exit 1
