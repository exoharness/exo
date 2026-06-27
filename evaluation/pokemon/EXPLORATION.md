# Pokémon self-improvement exploration — lab notebook

## ⭐ DESIGN PIVOT (latest — supersedes the scaffolding work below)

Alex's directive: **do NOT hand-code game support into the harness.** No injected
coordinates / wall-maps / minimaps / "go north" / battle helpers — that's the harness
playing the game FOR the agent. The harness must be a generic SELF-IMPROVING substrate:
just the screen + self-improvement machinery (memory, build-your-own-tools, shell+self-edit)

- a minimal "learn to play" prompt. The agent discovers navigation/battles and builds its
  OWN tools. (See memory: feedback_pokemon_no_handcoding.)

Current harness (harness-pokemon-selfimprove.ts) = stripped to that. The ONLY non-game-
generic things provided: the current screen, the PREVIOUS frame (raw, so the agent can see
what its last action did — generic environment observation, not game knowledge), and the
self-improvement tools. Runner still records game RAM but ONLY for our scoring/live-view —
the agent never sees it.

### Findings from the principled runs (exp7–exp10), Route1 start

- **exp7** (intrinsic reflection, still had scaffolding): self-reflects unprompted (caught its
  own loop, "paused because looping") but lightly; died flailing in a wild battle.
- **exp8** (pure: minimal prompt, screen only, NO frame feedback): idled at the START for 39
  turns — frame-BLIND, literally couldn't perceive "nothing changed." BUT built itself a
  `png-ascii` screen-reader tool unprompted. $3, no progress.
- **exp9** (+ computed frame-diff "your last action changed X%"): moved off start by turn 4;
  learned + saved battle controls to memory unprompted; then 30-turn battle loop.
- **exp10** (+ RAW prev+current frames, no computed diff — Alex's suggestion): moved off start
  by turn 3; built `png-tile-view` tool when stuck; battle wall again. Confirms raw two-frames
  is enough — no need to spoon-feed a computed diff.

### Conclusions

1. **The key generic perception primitive = frame comparison.** Showing the previous frame next
   to the current one lets the agent see "my action did nothing → I'm stuck." Raw two-frames is
   sufficient (don't need a computed diff). This is generic env feedback, NOT game knowledge.
2. **With the minimal harness the agent reliably self-improves**: navigates from the screen,
   detects stuckness from the two frames, and when blocked builds its OWN screen-reading tools
   (ascii / tile-view) + learns mechanics into memory — zero hand-coded game support.
3. **Battles are the open wall.** No generic primitive rescues them: the battle screen ALWAYS
   changes (text/animation) so frame-comparison never flags the non-winning loop, and "am I
   winning" (enemy HP) is game-specific knowledge we won't inject. The agent gets stuck mashing
   A for 30 turns. This must be solved by the agent self-improving a battle reader — hard for
   gpt-5.5 from raw pixels. This is the current frontier.

---

## TL;DR FOR ALEX (read this first) — NOTE: scaffolding-era, see DESIGN PIVOT above

Goal: a good agent that plays Pokémon well, learns/improves, and trims cost.
What I found overnight (details below):

- **The OOM was uncleaned Docker sandboxes** (one per conversation) piling up. Fixed:
  delete old conv on reset + prune + a RAM/container watchdog (`safe_run.sh`). Box is safe now.
- **Built objective instrumentation**: read game RAM (map id, x/y, badges, party, money) so
  "progress" is measured, not guessed. Live view now shows a game-progress panel + minimap +
  the cumulative-spend curve.
- **The agent's #1 weakness is NAVIGATION**, not cost. Pure vision → it wanders/loops. I found
  the fix is a _ladder of ground-truth spatial feedback_, each layer added because the previous
  wasn't enough: position string → stuck counter → per-tile wall map → global minimap+frontier
  → explicit goal-direction. Each helped; see the table.
- **gpt-5.5 does NOT self-improve on its own here** (0 tools, 0 policy self-edits across all
  runs, despite strong prompting). Honest finding: in a fast per-turn loop it just answers; it
  needs either a forcing function (dedicated reflection turns) or the scaffolding handed to it.
  The cost win (one-shot answers, lean memory) DID materialize: ~$0.20/turn looping → ~$0.01-0.02.
- Cost analysis of the old $243 stuck run: 2.1 model calls/turn, and 26% of calls (cache misses
  from diary-memory + duplicate tools) ate 68% of spend. Leaner context is the real cost lever.
- **🎉 THE RECIPE WORKS: exp6 reached Viridian City (a new map) in 50 turns** — first run ever to
  make real map progress. It = spatial scaffolding (lets it play) + reflection turns (make it
  self-improve: built a tool + consolidated memory). Winning config = current
  harness-pokemon-selfimprove.ts + runner `--reflect-every 15`. Earlier runs all died on Route 1.

Open question I'm still chasing: can the agent reach Viridian City (next map) with goal-direction

- minimap? And what's the right way to get GENUINE self-improvement vs. me scaffolding it.

**UPDATE — self-improvement cracked:** the agent only self-improves when FORCED to. Adding
"reflection turns" (every 15 turns: no button press, must take one concrete self-improvement
action) → at the very first one it built a `pokemon_state_reader` tool (reads game.json,
summarizes position/exits). So the recipe for the user's vision is: dedicated reflection turns,
not just prompting during play.

**🎉 RESULT — exp6 REACHED VIRIDIAN CITY (new map!) at turn 50.** The full recipe works:
spatial scaffolding (position + per-tile wall map + minimap + frontier + goal direction) lets
the agent navigate, and reflection turns give genuine self-improvement (2 tools built + memory
consolidated to 2 facts). It crossed all of Route 1 — start (y20) → through both chokepoints
(y14, y12) → found the exit column (x11,y0) → entered Viridian. ~$1.8, 50 turns, 2 maps, 32
tiles. This is the first run to make a real map transition. Earlier runs (exp1-5) all died on
Route 1. The combination of BOTH — scaffolding to play + reflection to improve — is the answer.

Started 2026-06-26 (overnight autonomous session). Goal: find the prompt/harness setup
that best makes exo (gpt-5.5) **beat the game**, using self-improvement (tool creation,
tool use, policy self-edits) and cost-efficiency in service of that.

## Goal hierarchy (in priority order)

1. **BEAT THE GAME** — the target. Measured objectively from game RAM:
   - distinct map IDs visited (exploration / un-stuck-ness)
   - badges earned (popcount 0xD356)
   - party level sum, distance moved
2. **Self-improvement** — tools created _and reused_, policy self-edits that stick.
3. **Cost efficiency** — progress per dollar; cost-per-turn trend. (A means, not the end.)

Prior failure modes observed (see chat history / the $243 stuck run):

- Pure-vision navigation → agent loops, circling one area for ~1000 turns.
- Diary-style memory + pile of near-duplicate vision tools → busts prompt cache
  (26% of calls were cache-misses eating 68% of spend).
- Over-correcting toward "spend less" → agent stops self-improving entirely (0 tools/edits).

## Verified facts (2026-06-26)

- **Pokémon Red US RAM**: map=0xD35E, X=0xD362, Y=0xD361, badges=0xD356 (bitfield),
  party count=0xD163, party mon1 level=0xD18C, money=0xD347 (3-byte BCD). Confirmed:
  Oak's lab reads map=40; Route 1 state reads map=12. PyBoy `pb.memory[addr]` works.
- **Start state for experiments**: `pokemon_run2000_end.state` — starter L5, map 12
  (Route 1), party=1, 0 badges, free overworld movement. Best for exercising navigation.
- **OOM cause (the crash)**: exo creates a Docker sandbox per _conversation_; with
  conv-reset-every + overlapping/killed runs these accumulate (20 alive → OOM → reboot).
  `kill <pid>` does not reap them. Fix: `exo conversation delete` old conv on reset +
  prune exited containers + a RAM/container watchdog. ONE run at a time.

## Safety protocol (non-negotiable after the OOM)

- One run at a time. Never overlap.
- Watchdog: prune exited `exo-*` containers every 60s; KILL the run + alert if
  free RAM < 3Gi or running exo containers > 10.
- Cap experiment runs at ~150–200 turns. Watch `free -h` / `docker ps`.

## Progress metric (per run)

`maps_visited` (set size), `max_badges`, `level_sum`, `tiles_visited`, plus cost.
Headline score = badges, then maps_visited, then level_sum. Logged to session.json +
live state.json each turn.

## Key design decision: inject position

The navigation bottleneck is spatial memory. The runner now writes game.json (map,x,y,
party,badges,visited counts) every turn, and the harness injects current position +
"if x,y unchanged you were BLOCKED, change direction" each turn. Rationale: a human
player sees where they are; building the map of where they've BEEN + planning routes is
the skill the agent must still develop (memory/tools). Smoke (old prompt, NO position)
confirmed the failure: agent mashed "up" into a wall 4+ turns, pos frozen at y=20,
reasoning "keep heading north" — blind. v3 adds the position feedback.

## Experiments

- **smoke_safe** (OOM test, old prompt, no position): 10 turns, conv-reset-4. RAM steady
  22GB, containers freed on reset, clean finish. Agent stuck at wall (no position feedback).
- **exp1_v3** (RUNNING): v3 progress-first prompt + position injection. 150 turns,
  Route1 start, self-improve, conv-reset-40. Tests: does position feedback break looping?
  does it reach new maps? does it self-improve toward playing better?

### Candidate next variants (pick based on exp1 failure mode)

- if still wandering → runner injects explicit "stuck N turns" + nudge systematic sweep.
- if no self-improvement → require a navigation tool by turn ~10; give a concrete template.
- if tools are junk/dupes → in-prompt example of a good visited-tracker tool.
- if progress works → cost-focus variant (ASCII screen render) to show the spend curve bend.

| #       | variant                            |        turns |     maps | badges | lvlΣ |    $ | tools | edits | notes                                                                                                                                                                                                                        |
| ------- | ---------------------------------- | -----------: | -------: | -----: | ---: | ---: | ----: | ----: | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| smoke   | old, no pos                        |           10 |        1 |      0 |    5 | 0.10 |     0 |     0 | stuck at wall (blind)                                                                                                                                                                                                        |
| exp1_v3 | progress + position string         | ~16 (killed) |        1 |      0 |    5 | 0.20 |     0 |     0 | **pinned on 1 tile, pressed "up" 16x.** Soft "you may be blocked" ignored; "go north" prior dominates                                                                                                                        |
| exp2    | + stuck counter                    | ~19 (killed) |        1 |      0 |    5 |    — |     0 |     0 | un-froze (moved off start by t3) but trapped oscillating on y=20 row; still re-pressed "up" while stuck                                                                                                                      |
| exp3    | + per-tile WALL MAP                | ~39 (killed) |        1 |      0 |    5 |    — |     0 |     0 | escaped y=20 row → reached y=14, then hard-stuck at (14,14) ~24 turns (local min)                                                                                                                                            |
| exp4    | + global minimap+frontier          | ~29 (killed) |        1 |      0 |    5 |  0.5 |     0 |     0 | drifted east/south (northmost y=20); aimless — no goal direction                                                                                                                                                             |
| exp5    | exp4 + GOAL=north                  | ~67 (killed) |        1 |      0 |    5 | 1.77 |     0 |     0 | **best nav: reached y=12, cleared 2 chokepoints**, then hard-stuck ~20 turns. Slow. Still 0 self-improve                                                                                                                     |
| exp6    | exp5 + REFLECTION turns (every 15) |          100 | **2** ✅ |      0 |    5 | 5.69 |   1-2 |     0 | **🎉 REACHED VIRIDIAN @ turn 50**, then explored it. 59 tiles, 59% of moves succeeded (vs ~0% frozen early runs). Built a state-reader tool + 5 genuinely-useful learned memories incl. the Oak's-Parcel story gate. WINNER. |

### exp6 final — the winning recipe, in detail

Config: current `harness-pokemon-selfimprove.ts` + runner flags `--self-improve
--conv-reset-every 40 --reflect-every 15`, from `pokemon_run2000_end.state`.

- **Progress (goal #1): reached Viridian City** — 2 maps, 59 tiles, 59% move-success.
  First run to ever change maps; exp1-5 all died on Route 1.
- **Genuine learning (not just navigating):** its 5 consolidated memories are real, e.g.
  "Route 1: near the top edge, up at x=14 is blocked by trees — weave west"; "Viridian:
  go north along x=21 to y=30, then step left"; and crucially **"Viridian's north exit is
  blocked until Oak's Parcel is obtained from the Poké Mart."** It correctly diagnosed the
  STORY GATE stopping it — that's understanding, not just wall-bumping. (This is also why it
  couldn't leave Viridian: it needs the Parcel errand, which is a multi-step sub-quest.)
- **Cost: $5.69 / 100 turns, and per-turn cost ROSE** ($0.024 → $0.060). Honest tradeoff:
  reflection turns + tool calls are an investment that buys progress + learning, not savings.
  The "spend curve flattens" story only holds for pure play; self-improvement costs money.

## THE ANSWER (what makes a good agent here)

1. **To play well:** it needs ground-truth spatial feedback, layered — position, per-tile
   walls, a running minimap+frontier, and a goal direction. Pure vision alone → it wanders.
2. **To learn/improve:** it does NOT self-improve spontaneously; **dedicated reflection turns**
   (step back, no button, make one concrete improvement) reliably produce tools + real
   consolidated knowledge.
3. **Cost:** one-shot answers during play are cheap (~$0.01-0.02/turn); reflection/tools are
   the cost. Net: spend money to get smarter, not to get cheaper. Tune `--reflect-every` to
   trade cost for learning.
   Next frontier: the Oak's Parcel sub-quest (enter Mart, talk, return) — needs the agent to act
   on its own learned memory ("get the Parcel") rather than just navigate. Good next experiment.

### Iterative finding (the core lesson so far)

Navigation feedback has to be **ground-truth and concrete**, escalating:

1. Inject position string ("if x,y unchanged you're blocked") → IGNORED. Model's "go north" prior wins.
2. Runner computes stuck-counter + "your last move (up) did NOT move you" → un-freezes, but still
   re-presses the blocked direction; gets trapped oscillating on one row.
3. Runner tracks **per-tile blocked directions** (any dir pressed on a no-move turn = confirmed wall
   at that tile) and tells the agent which dirs are walls vs untried from THIS tile. ← testing now.
   The pattern: the model won't reliably infer spatial facts; give it the fact directly (like the
   game's visual "bonk"), and leave the _strategy_ (routes, planning, tools) as the self-improvement.

### Route 1 geometry (offline probe, ground truth)

From the start (map12, x15,y20): pure "up" is fully walled. Best single-column northward
reaches only y=14 (via x=5) — then ANOTHER wall. No column walks straight to Viridian; the
path zigzags and y=14 is a chokepoint requiring a lateral move. So reaching the next town is
genuine multi-leg navigation, not a straight line. exp4 (minimap+frontier) is the test of
whether global map awareness lets it weave through. The minimap '?' frontier should point to
the lateral opening once it's explored near y=14.

### Route 1 ground truth (407-node offline BFS over the emulator)

From the start (map12, x15,y20), reachable tiles span y:0..35, x:4..17. Two map exits:

- **Pallet Town (map 0): ~20 steps** (south).
- **Viridian City (map 1): ~36 steps** (north — the forward/story direction).
  So "go north to Viridian" is correct and achievable in ~36 good steps, but it's the longer
  exit and passes a chokepoint around y=14 that needs a lateral weave. (A shallow BFS that
  stopped at the first exit had misled me into thinking only south/Pallet was reachable.)
  Implication: reaching Viridian is a legit but demanding ~36-step navigation for the LLM.

### exp4 (minimap + frontier injection)

Runner builds an ASCII map (visited/walls/frontier) per map id and injects it + nearest
unexplored exits each turn. This is navigation scaffolding (legit for "play well"; real
Pokémon agents have maps). NOTE the open tension: heavy scaffolding makes the agent play
better but does its navigating FOR it — tools/edits still 0. Self-improvement showcase may
need a separate, more forcing variant; the honest finding so far is gpt-5.5 does NOT
proactively build tools/self-edit in this loop, even when prompted to.
