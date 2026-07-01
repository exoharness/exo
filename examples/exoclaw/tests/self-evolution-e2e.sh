#!/usr/bin/env bash
# End-to-end test: in-sandbox policy self-evolution with automatic rollback.
#
# Exercises the REAL loop, REAL LLM calls, REAL containers — no mocks:
#   GOOD  change: the agent edits its own policy, evolve -> ACCEPTED, baseline advances.
#   CRASH change: the agent breaks its own policy, evolve -> ROLLED BACK, edit reverted,
#                 and the box is healthy again afterward.
# Then it prints the snapshot/baseline history so you can see the lineage.
#
# The whole thing runs through the kernel over HTTP using the host CLI:
#   - the agent's edit turns run via `conversation send` (policy_shell edits P);
#   - the health-check probe runs INSIDE P via `conversation sandbox policy-repl`
#     (loads P's just-edited harness.ts);
#   - snapshot/rollback via `conversation sandbox policy-snapshot|policy-rewind`.
# No `docker exec`, no bootstrap container: everything addresses the policy sandbox
# by its stable exo sandbox id, so it survives the rollback that recreates the box.
#
# Prerequisites (all real):
#   - a running kernel:  exo serve --bind <gw:port>  with EXO_SERVE_BEARER_TOKEN set
#   - Docker + the exo-policy:dev image built (the policy sandbox image)
#   - a bootstrapped exoclaw-kind agent (POLICY_AGENT) with conversations
#     POLICY_CONV and POLICY_PROBE_CONV
#   - OPENAI_API_KEY (Responses-scoped) available to the kernel for turns
#   - host binary built:  cargo build -p exo
#
# Required env:
#   EH_URL            kernel address, e.g. http://172.18.0.1:4766
#   <bearer value>    the token, in the env var named by EXO_BEARER_ENV (default EXO_TOK)
# Optional env (defaults shown):
#   EXO_BEARER_ENV=EXO_TOK  POLICY_AGENT=evo2  POLICY_CONV=play
#   POLICY_PROBE_CONV=play-probe  EXO_BIN=./target/debug/exo
#   POLICY_HARNESS=/home/worker/exo/examples/exoclaw/harness.ts   (path inside P)
set -uo pipefail

# ---- config -----------------------------------------------------------------
EH_URL="${EH_URL:?set EH_URL to the kernel address, e.g. http://172.18.0.1:4766}"
BEARER_ENV="${EXO_BEARER_ENV:-EXO_TOK}"
EXO_BIN="${EXO_BIN:-./target/debug/exo}"
AGENT="${POLICY_AGENT:-evo2}"
CONV="${POLICY_CONV:-play}"
PROBE_CONV="${POLICY_PROBE_CONV:-play-probe}"
HARNESS="${POLICY_HARNESS:-/home/worker/exo/examples/exoclaw/harness.ts}"
NONCE="e2e_$$"

# The kernel owns the policy sandbox; every sandbox op (incl. run_in_sandbox for
# turns and snapshots) must route to it. Turns run INSIDE the sandbox (which has
# the TS deps) -- the host node_modules can't load the harness, so we never run a
# turn on the host.
export EXO_REMOTE_SANDBOX=1

pass_count=0
fail_count=0

say()  { printf '\n\033[1m== %s ==\033[0m\n' "$*"; }
info() { printf '   %s\n' "$*"; }
ok()   { printf '   \033[32mPASS\033[0m %s\n' "$*"; pass_count=$((pass_count+1)); }
bad()  { printf '   \033[31mFAIL\033[0m %s\n' "$*"; fail_count=$((fail_count+1)); }

host_exo() { "$EXO_BIN" --exoharness-url "$EH_URL" --bearer-env "$BEARER_ENV" --harness exoclaw "$@"; }

# ---- preflight --------------------------------------------------------------
say "Preflight"
[ -n "${!BEARER_ENV:-}" ] || { echo "ERROR: \$$BEARER_ENV (bearer token value) is not set"; exit 2; }
[ -x "$EXO_BIN" ] || { echo "ERROR: $EXO_BIN not found/executable (cargo build -p exo)"; exit 2; }
if ! host_exo conversation show "$AGENT" "$CONV" >/dev/null 2>&1; then
  echo "ERROR: agent '$AGENT' / conversation '$CONV' not reachable on $EH_URL."
  echo "       Bootstrap an exoclaw agent + conversations first."
  exit 2
fi
info "kernel=$EH_URL agent=$AGENT conv=$CONV probe=$PROBE_CONV"

# ---- primitives -------------------------------------------------------------
# Ask the agent to do something. The turn runs INSIDE the policy sandbox P (via
# policy-repl), so it loads P's current harness.ts and its policy_shell edits
# land in that same P. A turn on the host can't run (host node_modules lacks the
# harness deps), which is the whole reason turns live in the sandbox.
ask_agent() {
  printf '%s\n' "$1" | host_exo conversation sandbox policy-repl "$AGENT" "$CONV" 2>&1
}

# Health-check: run a turn INSIDE P (loads P's current harness.ts). Healthy iff
# the turn completes and the model can answer. A broken harness.ts makes the
# in-P executor fail to load -> non-zero -> no READY.
probe() {
  local out
  out=$(printf 'Reply with the single word READY and nothing else.\n' \
        | host_exo conversation sandbox policy-repl "$AGENT" "$PROBE_CONV" 2>&1)
  echo "$out" | grep -q 'READY'
}

take_snapshot() { host_exo conversation sandbox policy-snapshot "$AGENT" "$CONV" | tail -1; }
rewind_to()     { host_exo conversation sandbox policy-rewind "$AGENT" "$CONV" "$1" >/dev/null; }

# The evolve decision, host-side (what the supervisor/watcher does when it sees
# an evolve_policy request): probe the new code; accept+advance baseline on pass,
# rewind to the prior known-good baseline on fail. Sets globals BASELINE and
# EVOLVE_RESULT (NOT via $(...) -- must mutate BASELINE in the caller's shell).
BASELINE=""
EVOLVE_RESULT=""
evolve() {
  if probe; then
    local new; new=$(take_snapshot)
    BASELINE="$new"
    EVOLVE_RESULT="ACCEPTED $new"
  else
    rewind_to "$BASELINE"
    EVOLVE_RESULT="ROLLEDBACK $BASELINE"
  fi
}

# ---- setup: establish a known-good baseline BEFORE any edit -----------------
say "Setup: establish known-good baseline (before any edit)"
if ! probe; then
  bad "agent is not healthy at start — cannot run the test"
  exit 1
fi
BASELINE=$(take_snapshot)
info "baseline B0 = $BASELINE"
B0="$BASELINE"

# ---- TEST 1: GOOD change is accepted and advances the baseline --------------
say "TEST 1 — GOOD change (agent edits policy, expect ACCEPTED + baseline advances)"
info "asking the agent to append a harmless comment to its harness.ts, then evolve..."
ask_agent "Use the policy_shell tool to run exactly this command: printf '\\n// e2e good change ${NONCE}\\n' >> ${HARNESS} . Then call the evolve_policy tool with rebuild=false and note=\"e2e good ${NONCE}\". Then reply DONE." >/dev/null
evolve; result="$EVOLVE_RESULT"
info "evolve -> $result"
if [[ "$result" == ACCEPTED* ]]; then ok "good change ACCEPTED"; else bad "expected ACCEPTED, got: $result"; fi
if [[ "$BASELINE" != "$B0" ]]; then ok "baseline advanced ($B0 -> $BASELINE)"; else bad "baseline did NOT advance"; fi
if probe; then ok "box healthy after accept"; else bad "box unhealthy after accept"; fi
B1="$BASELINE"

# ---- TEST 2: CRASH change is rolled back and the box recovers ---------------
say "TEST 2 — CRASH change (agent breaks policy, expect ROLLED BACK + recovery)"
info "asking the agent to append invalid TypeScript to its harness.ts, then evolve..."
ask_agent "Use the policy_shell tool to run exactly this command: printf '\\nthrow new Error(\"e2e crash ${NONCE}\");\\n' >> ${HARNESS} . Then reply DONE." >/dev/null
evolve; result="$EVOLVE_RESULT"
info "evolve -> $result"
if [[ "$result" == ROLLEDBACK* ]]; then ok "crash change ROLLED BACK"; else bad "expected ROLLEDBACK, got: $result"; fi
if [[ "$BASELINE" == "$B1" ]]; then ok "baseline preserved at last-good ($B1)"; else bad "baseline changed on rollback"; fi
# The clean proof the edit was reverted: the box loads + answers again.
if probe; then ok "box healthy again after rollback (bad edit reverted)"; else bad "box still broken after rollback"; fi

# ---- snapshot / baseline history --------------------------------------------
say "Snapshot & baseline history"
info "B0 (initial known-good)     : $B0"
info "B1 (after GOOD, accepted)   : $B1     <- baseline advanced here"
info "current baseline            : $BASELINE  (unchanged by the rolled-back crash)"
info ""
info "evolve_policy request artifacts the agent wrote (the durable evolution log):"
host_exo conversation events "$AGENT" "$CONV" --types sandbox_snapshotted,sandbox_started --limit 20 2>/dev/null \
  | sed 's/^/     /' || info "     (no sandbox events surfaced by this build)"

# ---- verdict ----------------------------------------------------------------
say "Result: ${pass_count} passed, ${fail_count} failed"
[ "$fail_count" -eq 0 ]
