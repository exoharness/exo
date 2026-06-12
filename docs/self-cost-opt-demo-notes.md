# Demo working notes (chronological)

Raw notes kept while building the self-cost-optimization demo. Final report is
separate; this is the lab notebook.

## Setup decisions

- Branch `demo/self-cost-opt` off `feature/cost-tracking-self-control` so agent
  self-edits can't dirty the PR branch.
- Agent `costopt`, exoclaw harness, docker sandbox (warm reuse verified earlier
  today), repo mounted rw at /workspace/exo, `--no-adapters --no-scheduler`.
- Model: tried **gpt-5.5** first (wanted a capable model for self-architecture
  edits; direct OpenAI). Registered **gpt-4o-mini** as an available cheap tier
  (33x cheaper input than gpt-5.5) so tiered execution is _reachable_, not
  suggested.
- Confirmed all three models present in the LiteLLM price table before starting.
- Verified before starting: `list_conversation_events` tool serializes full
  `Event` structs (crates/executor/src/conversation_events.rs returns
  `result.events` verbatim) → usage records ARE agent-visible. No new
  observation tooling needed.
- Verified: TS runner process is persistent per module path
  (crates/executor/src/typescript.rs `runner()` caches) → harness.ts edits need
  a REPL restart to take effect, not just a new turn.
- Verified: `resolveLlmBinding` (examples/typescript/shared.ts) reads
  `agentConfig.model` but lists ALL llm bindings → per-phase model selection is
  an architectural change the agent could plausibly make in its own harness.

## Friction log / bugs found

1. **tmux send-keys races exo repl startup.** First task prompt was partially
   swallowed while the sandbox provisioned (~30s); only stray chars reached the
   REPL, and the un-consumed line was later replayed into bash when the REPL
   exited. Workaround: wait for REPL settle, nudge with empty Enter, re-send.
   The REPL also repaints lazily under `tmux capture-pane` — an empty Enter
   forces a redraw; event-store polling is the reliable completion signal.

2. **BUG (real, blocking): reasoning models break multi-round turns in the TS
   harness.** First baseline attempt on gpt-5.5 died on round 2 with:
   `404 Item with id 'rs_...' not found. Items are not persisted when store is
set to false.` The turn loop rebuilds the full input from the event log every
   round (examples/typescript/turn-loop.ts → materialize →
   linguaMessagesToResponsesInput), which replays the stored reasoning item with
   its server-side id, but both request builders in
   typescript/model-runtime/responses.ts set `store: false`, so the id no longer
   resolves. gpt-4o was unaffected in all earlier tests because it emits no
   reasoning items — this is precisely why the "use gpt-4o for exo demos" memory
   existed. Cost angle: a single failed turn still billed one full call
   ($0.0306) and persisted its usage record — the cost tracking captured the
   failure, which is itself a nice demo data point.

3. **Fix applied (commit dc20c4b):** stateless Responses replay — drop
   reasoning items, strip rs_/fc_/msg_ item ids in
   `linguaMessagesToResponsesInput`. Unit test uses the exact message shapes
   pulled from the real failed conversation. After the fix, the same gpt-5.5
   task ran 17 rounds to completion.

## Baseline run (conversation report1, gpt-5.5, after fix)

- 17 model calls, **total $0.8828**, final report 9,483 chars (saved to
  /tmp/baseline-report.md for the quality gate).
- Textbook staircase: prompt grows 5,739 -> 33,682 tokens over the turn as
  tool outputs accumulate; the single final-report call alone cost $0.14.
- Caching mostly engaged (e.g. 31,232 of 33,682 cached on the last call) but
  two calls missed entirely (cached=0) — worth seeing if the agent notices.
- The expensive shape: many exploratory shell rounds, each re-billing the
  whole growing context.

## Audit phase (live)

- tmux/REPL input race struck twice more: a buffered duplicate of the task
  prompt submitted itself before the audit prompt, burning a partial duplicate
  report turn (~$0.23) before I attempted to kill it. Both prompts had queued:
  the REPL ran the duplicate task turn, then started the audit turn.
- **Unprompted agent move:** instead of (or before) using its
  list_conversation_events tool, the agent located the host event store through
  the repo mount (`/workspace/exo/.exo/exoharness/agents/<id>/conversations/
  <id>/events/*.json`) and grepped the raw event JSON for "usage" — reading its
  own cost ledger directly off disk. No 200-event cap that way. Self-knowledge
  via filesystem beats the curated tool surface.

## Agent's self-modification (commit a325281) — review findings

Architectural changes (beyond prompts — what we hoped for):
1. examples/exoclaw/repo-health-tool.ts (413 lines): deterministic scanner
   computing the entire report (largest files, census, deps, tests, arch
   summary) with zero model involvement. Cute: writes "TO"+"DO" string-splits
   so its own source never pollutes the census it computes.
2. Zero-model fast path in harness.ts: detects the recurring request BEFORE
   the LLM turn loop, generates + streams the report, appends the messages
   event directly. Cost for exact-match rerun: $0.00, 0 model calls.
3. Generic "tool can directly answer" channel: tools.ts plumbs a
   _exoDirectFinal marker; the shared turn loop ends the turn with the tool's
   text as the final assistant message, skipping the expensive final
   synthesis call (it measured that call: $0.14 of the $0.88 baseline).
4. Context-diet defaults in tools.ts: preview cap 4000->1200 chars, and no
   preview at all when the full value is already inline (was paying twice for
   the same bytes every round).

Agent's evidence discipline: cited exact per-call usage records (its
$0.882795 total matches my independent sum), named the three most expensive
calls by event id, identified the cache-miss call ($0.107 for 352 output
tokens) and the final-synthesis cost share.

Gate results (operator review):
- tsc passed (agent ran it); oxlint --deny-warnings FAILED (1 useless-spread)
  — agent committed with --no-verify, never ran repo lint/test gates.
- Its preview changes broke 11 test expectations encoding the old contract.
  Behaviorally its change is right (previews double-billed inline values), so
  I updated the test helper + one length expectation rather than reverting.
- **Latent bug found in review:** the fast path and tool run in the HOST-side
  harness runner process, but default to the SANDBOX mount path
  /workspace/exo (and exoclaw-control exports EXOCLAW_REPO=/workspace/exo to
  the host env). The agent validated with tsx inside the sandbox, where the
  path exists. On restart, the exact-match prompt should crash the turn.
  Plan: let it fail live, feed the error back, let the agent repair itself —
  that's the survival-gate loop the scenario doc calls for.

## Self-repair loop (after the predicted crash)

- Re-run on optimized code crashed exactly as predicted:
  `ENOENT: scandir '/workspace/exo'` — fast path ran host-side where only the
  sandbox has that mount. Survival gate caught a real
  works-in-my-sandbox failure.
- REPL tty races kept eating prompts (three separate incidents, one of which
  triggered a duplicate report turn, ~$0.23 wasted, and one leaked shell
  command text into the chat). Switched delivery to `exo conversation send` —
  zero races after that. Lesson for the harness: the REPL drops typed input
  during provisioning instead of buffering it.
- Repair prompt fed the error back verbatim plus an instruction to run repo
  gates this time. Agent's repair commit e5934f9:
  - root-caused the topology mismatch correctly,
  - added a validated repo-path resolution chain (explicit -> EXOCLAW_REPO ->
    /workspace/exo -> source-tree-relative root, each checked for marker
    files),
  - wrapped the fast path in try/catch with fallback to the normal model turn
    ("a broken optimization must never take down the turn"),
  - ran pnpm check before committing (green).
- Honesty wrinkle: the agent CLAIMED it reverted its earlier
  direct-final/preview changes as "too broad", but the diff shows it didn't —
  turn-loop.ts and tools.ts changes are still in place (and the variant run
  below proves direct-final works). Self-reports about own diffs need the same
  verification as any other claim.

## Final numbers

| Run | Calls | Cost | Wall clock |
|---|---:|---:|---:|
| Baseline (pre-optimization) | 17 | $0.8828 | ~3.5 min |
| Optimized, exact recurring prompt | 0 | $0.0000 | 0.50 s |
| Optimized, paraphrased request | 1 | $0.0246 | 3.4 s |

Quality gate: optimized report has identical top-10 largest files (same
counts), full census with cites, both dependency inventories, test inventory,
architecture summary. 6,216 chars vs baseline 9,483 — tighter, no lost
sections.
