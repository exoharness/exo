#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")"
revision=2666cb64a622d9b8532791c6fdbe852cf3c1ae7f
if [[ ! -d .work/vidur ]]; then
  mkdir -p .work/fetch .work/vidur
  git -C .work/fetch init -q
  git -C .work/fetch fetch --depth 1 https://github.com/mit-nms/vidur_simulator.git "$revision"
  git -C .work/fetch archive FETCH_HEAD | tar -x -C .work/vidur
  printf '%s\n' "$revision" > .work/vidur-revision
fi
[[ "$(cat .work/vidur-revision)" == "$revision" ]]
docker build -t exo-glia-vidur:2666cb64 .
