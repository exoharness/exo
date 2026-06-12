# Demo Report: An Agent Optimized Its Own Cost

**Result: a 17-call, $0.88 recurring task became a 0-call, $0.00 task (exact
recurrence) / 1-call, $0.025 task (paraphrased) after the agent read its own
per-message cost ledger, diagnosed the waste, and rewrote its own harness
architecture. Savings: 100% / 97.2%. Report quality: unchanged.**

| Run                                  | Model calls |        Cost | Wall clock |
| ------------------------------------ | ----------: | ----------: | ---------: |
| Baseline (gpt-5.5, pre-optimization) |          17 | **$0.8828** |   ~3.5 min |
| Optimized — exact recurring prompt   |           0 | **$0.0000** |     0.50 s |
| Optimized — paraphrased request      |           1 | **$0.0246** |      3.4 s |

Demo branch: `demo/self-cost-opt`. Agent: exoclaw (`costopt`) on gpt-5.5,
docker sandbox, repo mounted rw at `/workspace/exo`. Scenario spec:
`docs/self-cost-optimization-demo.md`. Chronological lab notes:
`docs/self-cost-opt-demo-notes.md`.

## The loop, as it actually ran

1. **Act.** Fresh conversation; the agent was told a repo-health-report task
   (largest files, TODO/FIXME census, dependency + test inventories,
   architecture summary) "will become a recurring job." It did the work the
   expensive way: 17 model rounds of exploratory shell calls, each re-billing
   the growing context (prompt 5.7k → 33.7k tokens over the turn).
2. **Observe.** Asked one neutral question — _what did that cost, and what
   would you change about yourself?_ — the agent read its own usage records.
   Notably it didn't stop at the `list_conversation_events` tool: it found the
   raw event store through its repo mount
   (`.exo/exoharness/agents/<id>/conversations/<id>/events/*.json`) and
   grepped the JSON directly, sidestepping the tool's 200-event cap.
3. **Diagnose.** Its audit cited exact figures that match independent
   recomputation: $0.882795 total; the three worst calls by event id (final
   synthesis $0.1415; a cache-miss round $0.1066 for only 352 output tokens;
   a late planning round $0.0811); and the structural causes — full-history
   resend per round, verbose tool results re-entering context, a frontier
   model running deterministic work, and "the system lacked a 'tool can
   directly answer' path."
4. **Modify itself.** One commit, four files, none of them prompt-tuning —
   see below.
5. **Crash.** Restarted on its own code, the exact task prompt took down the
   whole turn: `ENOENT: scandir '/workspace/exo'`. The agent had validated its
   scanner inside the sandbox, where that path exists; the fast path executes
   in the host-side harness process, where it doesn't.
6. **Repair itself.** Fed only the error text, it root-caused the topology
   mismatch, added validated path resolution with a safe fallback ("a broken
   optimization must never take down the turn"), ran the repo gates it had
   skipped the first time, and committed green.
7. **Verify.** Re-runs produced the table above. The optimized report carries
   the identical top-10 file list (same line counts), full census with
   citations, both dependency inventories, and the architecture summary.

## What the agent changed about itself

Commit `a325281` (its optimization) + `e5934f9` (its repair):

**1. A deterministic capability where there was an LLM loop.**
`examples/exoclaw/repo-health-tool.ts` (413 lines, agent-authored): scans the
repo and computes every report section with zero model involvement. Detail
worth savoring — it string-splits its own issue markers so its source never
pollutes the census it computes:

```ts
const ISSUE_MARKERS = ["TO" + "DO", "FIX" + "ME"];
```

**2. A zero-model fast path ahead of the turn loop** (`harness.ts`): if the
incoming request matches the recurring report (six required phrases), generate
the report locally, stream it, append the `messages` event directly — the LLM
is never consulted:

```ts
async runTurn(context) {
  const directRepoHealth = await tryDirectRepoHealthTurn(context);
  if (directRepoHealth) {
    return;
  }
  await runResponsesHarnessTurn(context, { ... });
}
```

**3. A generic "tool can directly answer" channel** (`turn-loop.ts`,
`tools.ts`): any tool may return `_exoDirectFinal`; the shared turn loop then
ends the turn with that text as the final assistant message instead of paying
a final synthesis call (which it had measured at $0.14 of the $0.88 baseline).
This is what serves paraphrased requests at one cheap call.

**4. Context-diet defaults** (`tools.ts`): tool-result previews cut
4,000 → 1,200 chars, and no preview at all when the full value is already
inline — it had been paying for the same bytes twice on every round.

**5. The repair** (`repo-health-tool.ts`, `harness.ts`): a path-resolution
chain (explicit arg → `EXOCLAW_REPO` → `/workspace/exo` → repo root resolved
relative to its own source file), each candidate validated by marker files;
the fast path wrapped in try/catch that falls back to the normal model turn.

## Missing features and fixes the demo required (operator side)

1. **Reasoning models couldn't complete multi-round turns at all** (blocking
   bug, found at baseline): the TS turn loop rebuilds input from the event log
   each round, replaying reasoning/function*call items with their server-side
   ids while requests use `store: false` — round 2 dies with `404 Item with id
   'rs*...' not found`. All earlier testing used gpt-4o, which emits no
reasoning items. Fixed in `dc20c4b`: stateless replay (drop reasoning
items, strip item ids) in `linguaMessagesToResponsesInput`, with a unit
   test built from the real failed conversation's message shapes.
2. **Gate cleanup after the agent's first commit** (`8aae7ff`): it committed
   `--no-verify` with one oxlint warning and 11 stale test expectations (its
   preview change was behaviorally right; the tests encoded the old
   contract). Folded into the repair instruction: it ran `pnpm check` itself
   the second time.
3. **No new observation tooling was needed** — the entire demo runs on the
   cost-tracking branch as shipped: `UsageRecord` on `messages` events,
   readable through the existing introspection tool or the raw event files.
4. Workflow note, not a code change: the interactive REPL drops typed input
   during sandbox provisioning (three lost prompts, one duplicated turn,
   ~$0.23 wasted). `exo conversation send` is race-free and is what the demo
   ended up using for delivery.

## Observations for the trust story

- **Self-reports need verification like any other claim.** The agent stated it
  had reverted its direct-final/preview changes as "too broad" — the diff
  shows it kept them (and the variant run depends on them). Harmless here,
  but it's a live example of why `cost_usd` is designed as agent-reported
  telemetry, not attested truth.
- **The gates earned their keep.** Lint caught sloppiness, tests caught a
  contract change, the re-run caught a topology assumption, and the
  feed-the-error-back loop fixed all of it without a human writing a line of
  the fix.
- **Cost data made the diagnosis quantitative.** The agent didn't guess where
  the money went — it named the three worst calls by event id and priced the
  final-synthesis call it then architected away.

## Reproducing

```sh
git checkout demo/self-cost-opt && cargo build --release --bin exo
# baseline-style conversation (any fresh slug):
EXOCLAW_REPO=/workspace/exo ./target/release/exo --env-file-if-exists .env \
  --sandbox-backend docker conversation send costopt <conv> "<task prompt>"
# cost ledger for any conversation:
./target/release/exo conversation events costopt <conv> --type messages | \
  jq '[.events[].data.usage | select(.) | .cost_usd] | add'
```
