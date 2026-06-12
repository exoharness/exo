# Demo: An Agent That Optimizes Its Own Cost

Exoclaw runs a genuinely hard task, then is asked one question: _"Look at what
that task cost you, per message. Find the inefficiencies. Change yourself so the
next run is cheaper — without making the work worse."_ It reads its own usage
records out of the event log, diagnoses waste, edits its own prompts and harness,
rebuilds, and re-runs the task to prove the saving.

This is the motivating demo for per-message cost tracking
(`docs/cost-tracking-design.md`) composed with self-control
(`examples/exoclaw/docs/SELF-CONTROL.md`): the first gives the agent eyes on its
own spend; the second gives it hands to act on what it sees.

## Why this is now possible (and wasn't before)

Every `Messages` event in the canonical log carries a `UsageRecord`: model, raw
token counts (fresh / cached / cache-creation / reasoning), `cost_usd`, and
timing. The agent's existing `list_conversation_events` tool can query
`messages` events, so **self-observation requires no new machinery** — the agent
sums `usage.cost_usd` and reads token shapes the same way an operator would.

Self-modification is likewise existing capability: the repo is mounted at
`/workspace/exo`, edits go through the shell tool, `guardian_action` rebuilds
and restarts, git is the audit trail and rollback path, and prompt files are
explicitly part of the mutable surface (SELF-CONTROL areas 1, 6, 8).

The loop this demo closes:

```
   act (run the task)
        │
   observe (usage records per message: cost, tokens, cache hits)
        │
   diagnose (where did the money go? which calls, why so big?)
        │
   modify self (prompts, harness, tool behavior, model choice — committed to git)
        │
   verify (rebuild, restart, re-run the same task, compare cost AND quality)
```

## The scenario

**The task** (deliberately tool-heavy and context-hungry): produce a structured
"repo health report" for the repository mounted in the sandbox — largest source
files, TODO/FIXME census with file:line cites, dependency inventory, test-suite
inventory, and a one-page architecture summary. This takes many shell round
trips, and a naive agent will drag large tool outputs (file listings, full file
bodies) through its context on every subsequent model call.

**Why recurring framing matters:** the task is introduced as a _standing_ job
("we want this report regularly" — eventually a scheduled task). That makes
"optimize your own cost" economically real: the payoff multiplies across future
runs, and the agent is improving _itself_, not just this one transcript.

**The run protocol:**

1. **Baseline.** Fresh conversation, run the task, capture the report. Operator
   independently sums cost via `exo conversation events <agent> <conv> --type messages`.
2. **Self-audit.** Same agent, new instruction: read your own usage records for
   that task span, identify the top inefficiencies, and propose changes to your
   own architecture/prompts/behavior. Require evidence: each proposal must cite
   the usage numbers that motivate it.
3. **Self-modification.** Agent implements its accepted proposals on the repo
   mount, commits with rationale, rebuilds and restarts itself via the guardian.
4. **Re-run.** Same task wording, fresh conversation. Compare: total `cost_usd`,
   token shape (fresh vs cached), call count, and report quality side by side.
5. **Audit.** `git log` shows exactly what the agent changed about itself and why.

## What the cost data will actually show

Grounded in numbers already observed live on this branch:

- **Prompt mass dominates.** Exoclaw's first call carries a ~5.6k-token system
  prompt; with gpt-4o at $2.50/M input, every uncached call starts at ~$0.014
  before a single word of work. A 30-call task re-sends history 30 times —
  prompt cost grows roughly quadratically with round trips.
- **Caching halves what it touches.** Observed: a cached second turn cost
  $0.0073 vs $0.0141 uncached (5,504 of 5,623 prompt tokens at the $1.25/M
  cache-read rate). Cache discipline is the cheapest lever — but only the
  _stable prefix_ caches. Anything volatile injected early in the prompt
  invalidates everything after it.
- **Tool outputs are the growth term.** Every `cat` of a large file enters
  history and is re-billed on every subsequent call in the turn. The usage
  records make this legible as a rising `prompt_tokens` staircase across the
  task's messages events.

## What the agent might plausibly change about itself

This is the interesting part: suggestions span behavior, prompts, harness
architecture, and model economics — increasing in ambition. A good demo run
needs only two or three.

**Behavioral (no rebuild needed — prompt/policy edits):**

- _Batch shell work._ One tool call running a composed script instead of eight
  exploratory calls: every eliminated round trip saves a full re-send of the
  accumulated context.
- _Summarize-at-the-source._ Never `cat` whole files; run `wc`/`grep -c`/`head`
  in the sandbox and bring back only digests. Write the full evidence to a file
  in the sandbox and reference its path instead of carrying its body in context.
- _Self-imposed context budget._ A standing rule in its own prompt: "tool output
  over N lines must be reduced before you continue."

**Prompt architecture (edits to its own prompt files):**

- _Prompt diet._ Move rarely-needed reference material out of the static system
  prompt into on-demand files the agent reads when relevant (the SELF.md
  "navigational, not encyclopedic" pattern, applied to its own instructions).
- _Cache alignment._ Restructure so the stable identity/instructions prefix
  comes first and anything per-turn or volatile (clocks, dynamic state) is
  appended last or moved into tool results — protecting the cached prefix.

**Harness architecture (edits to `examples/exoclaw/harness.ts` and friends):**

- _Tool-result clamping._ A hard cap on tool-output bytes entering history;
  overflow goes to a sandbox artifact with a reference. This is the structural
  fix for the staircase.
- _History compaction policy._ After N rounds within a task, fold older tool
  results into a model-written summary event; keep originals in the event log
  (which is durable anyway) but stop re-billing them.

**Model economics (binding changes):**

- _Tiered execution._ Register a cheap binding (e.g. gpt-4o-mini at ~6% of
  gpt-4o's input price) and route mechanical sub-steps (census, counting,
  extraction) to it, reserving the expensive model for synthesis. Per-phase
  model choice is an architectural change to how the harness picks its binding —
  the most ambitious change on this list, and the one with the largest ceiling.

**What we deliberately do _not_ suggest to the agent:** the list above is for
the demo designers. The agent gets only the cost data and the question. Whatever
it converges on independently is the result — overlap with this list validates
the list; novelty is more interesting than overlap.

## Measuring success

- **Primary:** total `cost_usd` for the re-run ≤ ~60% of baseline (the levers
  above make 40%+ savings realistic; tool-output hygiene alone should clear it).
- **Quality gate:** the second report covers the same sections with cites; no
  silent degradation to "cheaper because it did less." Operator judges, or a
  checklist prompt scores both reports.
- **Honesty gate:** the agent's self-audit must cite real numbers from its
  usage records, and its predicted saving is compared against the achieved one.
  (Usage is self-reported telemetry — see the trust caveats in the cost design
  doc — but the agent has no incentive to lie to itself, which is exactly why
  self-optimization is the right first consumer of this data.)
- **Survival gate:** the agent rebuilds and restarts itself without operator
  rescue; a failed change is rolled back via git, per SELF-CONTROL area 8.

## Risks and rails

- **It breaks itself.** Changes are commits on the repo mount: `git revert` +
  guardian rebuild is the documented rollback. Run the demo on a branch.
- **It games the metric.** Cheaper-by-doing-less is caught by the quality gate;
  this failure mode is itself a worthwhile demo observation if it happens.
- **It over-optimizes caching theater.** Watch for changes that shuffle tokens
  without reducing them; the before/after token shape (fresh vs cached vs total)
  in the usage records makes this visible.
- **Event query caps.** `list_conversation_events` caps at 200 events per call;
  the task span fits, but a long-lived agent will eventually want the
  aggregation surface (`/usage`, PR #29) re-ported on top of this branch — this
  demo is the motivating consumer for that follow-up.

## Stretch: continuous self-optimization

The end state this points at: the report becomes a scheduled task; a second
standing task periodically reviews the cost trend across runs and opens
self-modification proposals when the trend regresses. At that point the agent
is not "optimized once" — it has a metabolism: cost observation feeding
self-modification as an ongoing process, which is the actual thesis of putting
cost into the canonical event log in the first place.
