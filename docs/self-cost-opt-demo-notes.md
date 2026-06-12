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
