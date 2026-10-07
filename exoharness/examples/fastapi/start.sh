#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 2 ]]; then
  echo "Usage: $0 APP_PREVIEW_URL API_PREVIEW_URL" >&2
  exit 2
fi

export FRONTEND_HOST="${1%/}"
export VITE_API_URL="${2%/}"
export DATABASE_URL=postgresql://postgres:changethis@127.0.0.1:5432/app
export FASTAPI_ENV=development
export UV_PYTHON=3.14
export UV_PROJECT_ENVIRONMENT=/var/lib/fastapi-demo/venv
state=/var/lib/fastapi-demo
mkdir -p "$state/logs"

if [[ ! -f "$state/tools-ready" ]]; then
  export DEBIAN_FRONTEND=noninteractive
  apt-get update
  apt-get install -y --no-install-recommends postgresql-15 bubblewrap procps
  python3 -m pip install --break-system-packages uv
  uv python install 3.14
  npm install -g bun
  touch "$state/tools-ready"
fi

install -d -m 0700 -o postgres -g postgres "$state/postgres"
install -d -m 2775 -o postgres -g postgres /var/run/postgresql

if [[ ! -f "$state/postgres/PG_VERSION" ]]; then
  runuser -u postgres -- /usr/lib/postgresql/15/bin/initdb -D "$state/postgres" --auth-local=trust --auth-host=trust
fi
if ! runuser -u postgres -- /usr/lib/postgresql/15/bin/pg_ctl -D "$state/postgres" status >/dev/null; then
  runuser -u postgres -- /usr/lib/postgresql/15/bin/pg_ctl -D "$state/postgres" -l "$state/postgres/server.log" \
    -o "-h 127.0.0.1 -p 5432 -k /var/run/postgresql" -w start
fi
if [[ "$(psql -h 127.0.0.1 -U postgres -d postgres -tAc "SELECT 1 FROM pg_database WHERE datname='app'")" != 1 ]]; then
  createdb -h 127.0.0.1 -U postgres app
fi

cd /workspace/backend
uv sync --frozen
uv run bash scripts/prestart.sh
cd /workspace
bun install --frozen-lockfile

if ! curl --fail --silent --max-time 5 http://127.0.0.1:8000/api/v1/utils/health-check/ >/dev/null; then
  setsid bash -c 'cd /workspace/backend; exec uv run fastapi dev --host 0.0.0.0 --port 8000' \
    > "$state/logs/api.log" 2>&1 < /dev/null &
fi
if ! curl --fail --silent --max-time 5 http://127.0.0.1:5173/ >/dev/null; then
  setsid bash -c 'cd /workspace/frontend; exec bun run dev --host 0.0.0.0 --port 5173 --strictPort' \
    > "$state/logs/app.log" 2>&1 < /dev/null &
fi

for attempt in {1..60}; do
  if curl --fail --silent --max-time 5 http://127.0.0.1:8000/api/v1/utils/health-check/ >/dev/null \
    && curl --fail --silent --max-time 5 http://127.0.0.1:5173/ >/dev/null; then
    printf 'UI: %s\nAPI: %s/docs\nLogin: admin@example.com / changethis\nLogs: %s/logs\n' "$FRONTEND_HOST" "$VITE_API_URL" "$state"
    exit 0
  fi
  sleep 1
done
tail -n 60 "$state/logs/api.log" "$state/logs/app.log" >&2
exit 1
