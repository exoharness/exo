#!/usr/bin/env bash
# Generate Harbor task directories for SWE-bench Lite into this directory.
#
# Harbor's registry carries SWE-bench Verified but not Lite, so this fetches
# Harbor's swebench adapter at a pinned commit, patches it to accept a
# --dataset flag, and runs it against princeton-nlp/SWE-bench_Lite. The 300
# task directories it writes here are gitignored; only this recipe is tracked.
#
# Requires git and uv (uv fetches Python 3.13 and the swebench package).
set -euo pipefail

dataset_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
upstream="$dataset_dir/.upstream"
adapter="$upstream/adapters/swebench"
patch="$dataset_dir/adapter-dataset-flag.patch"

harbor_repo="https://github.com/laude-institute/harbor.git"
harbor_commit="71c77fdd119df12eb6ab56e5bc0f29bf62fad338"

if [[ ! -d "$upstream/.git" ]]; then
  git clone --filter=blob:none --sparse --no-checkout "$harbor_repo" "$upstream"
  git -C "$upstream" sparse-checkout set adapters/swebench
fi
git -C "$upstream" fetch --depth 1 origin "$harbor_commit"
git -C "$upstream" checkout --force --quiet "$harbor_commit"
git -C "$upstream" apply "$patch"

uv run --project "$adapter" swebench \
  --dataset princeton-nlp/SWE-bench_Lite \
  --output-dir "$dataset_dir" \
  --overwrite \
  "$@"
