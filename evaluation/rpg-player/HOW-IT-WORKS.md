# How the RPG Player Learns and Plays

A guide to what the agent actually experiences each turn, what persists
between turns, and where the learning levers are. File references are to
`agent/` unless noted.

## The one-sentence version

The agent is a loop that, every turn, rebuilds a prompt from a handful of
files it is allowed to edit, shows the model one screenshot, lets it call
tools about a dozen times, and then throws almost everything away except a
short self-written summary — so anything it wants to "know" tomorrow, it has
to write down today.

## Does it know it's playing Phantasy Star?

Yes. The fixed system prompt (`prompts/system.md`) opens with "You are an exo
agent playing Phantasy Star on a Sega Master System" and names the goals
(build the party: Myau, Odin, Noah), the control quirks (button2 = confirm,
PAUSE opens the menu), and the fact that dungeons are first-person mazes.

On top of that, the model has latent knowledge of Phantasy Star from
training — it has "read" walkthroughs. In practice that knowledge is spotty:
it may remember that Odin is a statue near Medusa but not the button path
through a shop menu, and it can confidently misremember map details. The
harness gives it no walkthrough, no map, and no RAM state. What it reliably
knows is what is in its prompt; what it half-knows is in its weights.

## Anatomy of a turn (`run.ts`)

1. **Fetch the opening frame** from the sidecar: a 3x-upscaled PNG plus a
   screen hash. There is no structured game state — the screenshot is the
   only ground truth.
2. **Assemble the prompt** (`context.ts`), in this order:
   - `prompts/system.md` — fixed rules, never changes (agent cannot edit).
   - **Playbook** (`runtime/playbook.md`) — the agent's own strategy notes,
     injected in full every turn. Max 16,000 chars. This is its main memory.
   - **Todos** (`runtime/todos.json`) — its persistent goal stack.
   - **Memory index** — only the _names and first lines_ of its memory
     files. Full contents cost a `read_memory` call.
   - **Skills index** — names + descriptions of installed skills; full
     instructions cost a `use_skill` call.
   - **Objective progress** — distinct-screens count and recent milestones,
     computed by the harness (the model cannot fake these).
   - **Recent turns** — its own summaries of the last 15 turns verbatim,
     plus milestone/improvement highlights from up to 60 older turns.
     Everything older is gone.
   - A directive, sometimes (see "Forcing function" below).
   - The screenshot.
3. **Tool loop**: up to **12 model round trips** per turn. Each round trip
   the model can call several tools; every game-affecting tool returns a
   fresh screenshot that is appended to the conversation, so within a turn
   it sees the consequences of each action. Tools:
   - Play: `press_buttons` (batched sequences), `wait`, `screenshot`
   - Rewind: `save_checkpoint`, `load_checkpoint`, `list_checkpoints`
   - Learn: `update_playbook`, `save_memory`/`read_memory`/`delete_memory`,
     `update_todos`, `install_tool`/`uninstall_tool` (writes real ES modules
     that hot-load into its own tool registry next turn),
     `install_skill`/`use_skill` (durable procedures in SKILL.md format)
   - Narrate: `claim_milestone` (logged as an unverified claim)
4. **Turn summary**: when the model stops calling tools, its final text
   becomes the turn summary (truncated to 600 chars). This summary — not
   the screenshots, not the reasoning — is what future turns will see.
5. **Bookkeeping** (`events.ts`): everything is appended to
   `runtime/events.jsonl` (the agent has no tool that can edit it), new
   screens update the novelty counter, milestones auto-save a checkpoint.

## What persists vs. what evaporates

| Survives across turns                    | Evaporates every turn     |
| ---------------------------------------- | ------------------------- |
| Playbook, todos, memories, skills, tools | All screenshots           |
| Turn summaries (last 15 verbatim)        | The model's reasoning     |
| Milestones + improvement markers         | Tool call results         |
| Checkpoints (save states)                | Anything not written down |

This is the crux of both the design and the "dumbness": the agent has severe
amnesia by construction, and the entire game is whether it compensates by
writing things down well. A human player remembers the shop layout after one
visit; this agent remembers it only if it put it in the playbook or a memory
file, in words that will make sense to a future self with no images.

## The forcing functions

Left alone, models under-invest in note-taking, so the harness pushes:

- **Bootstrap turn (turn 1)**: before any buttons are pressed, a directive
  instructs the model to dump everything it knows about Phantasy Star from
  its training data into memory files (quest chain, party recruitment,
  world layout, combat tips), marking unverified facts as such. This
  converts latent walkthrough knowledge into durable notes up front.
- **Reflection turn, every 10th turn**: a directive forbids button presses
  and instructs it to update playbook/memories/todos/tools/skills based on
  what it keeps re-learning. It also demands two audits: re-read one memory
  file as if it had never played (fix or delete what fails, upgrade
  confirmed "(unverified)" facts), and diagnose its biggest capability
  bottleneck — build it with install_tool if possible, otherwise spec it in
  a `harness_wishlist` memory. The wishlist is the agent telling us what
  the harness should grow next.
- **Stuck nudge**: if the opening screenshot is pixel-identical 3 turns in a
  row, a directive tells it bluntly that its approach is not working and
  suggests different buttons, a memory note about what does not work, or a
  checkpoint rewind.

## Objective progress

The only metric the model cannot inflate is **screen novelty**: the sidecar
hashes every frame, and the count of never-before-seen screens is shown each
turn (`progress.jsonl` logs threshold crossings: 10, 25, 50, 100...
distinct screens). Standing in a corner mashing buttons produces nothing;
opening a new menu, entering a new room, or advancing dialog produces new
screens. `claim_milestone` exists so it can assert story progress
("recruited Myau"), but claims are logged as claims.

## Why it looks dumb, specifically

Knowing the machinery, the failure modes are predictable:

1. **Vision-only position sense.** No coordinates, no facing indicator. In
   Phantasy Star's tile world, "did I actually move?" must be inferred from
   pixels; in first-person dungeons it is worse — every corridor looks the
   same, and one missed "I turned left" ruins its mental map.
2. **Default effort is cheap.** `RPG_MODEL=gpt-5.4` at
   `RPG_REASONING=low` — fine for pressing A through dialog, weak for maze
   reasoning and planning.
3. **The 12-round-trip budget** ends turns mid-thought; the plan survives
   only if the summary (600 chars) captures it.
4. **Empty seed playbook.** By design it starts knowing _how to learn_ but
   nothing about _the game_ beyond the system prompt and its weights. The
   turn-1 bootstrap directive (see "Forcing functions") now mitigates this
   by eliciting the model's latent walkthrough knowledge before play
   starts.

## Levers to make it learn faster

Ordered roughly by effort:

1. **Turn up the model** — `RPG_REASONING=medium|high`, or a stronger
   `RPG_MODEL`. Cheapest possible experiment.
2. **Seed the playbook** (`prompts/playbook.seed.md`) with game knowledge:
   controls, the opening quest chain, town list. Trades away the "learns
   from nothing" purity for a lot of early competence. (The implemented
   middle ground is the turn-1 bootstrap directive, which elicits the
   model's own latent knowledge instead of handing it external notes.)
3. **Raise the budgets** — more round trips per turn, a longer summary
   limit, more verbatim recent turns. Costs tokens, reduces amnesia.
4. **Pre-install a dungeon-mapping tool/skill** — a tool that maintains a
   grid map from the agent's "moved forward / turned left" reports and
   renders it as ASCII. Mapping-by-prose is its weakest skill; giving it
   scaffolding here attacks the hardest part of this specific game.
5. **Give it position sense (RAM probe)** — genesis_plus_gx exposes the
   emulated RAM in the WASM heap; a small sidecar probe reading player
   x/y/map-id (community RAM maps exist for Phantasy Star) would restore
   the PyBoy-style "did I actually move" ground truth and much stronger
   stuck detection. Most work, biggest structural payoff.
6. **Keep the last N screenshots in context** across turns (not just within
   a turn) so it can see its own recent trajectory.

My suggested order to try: 1, then 4, then 5 — 2 and 3 anytime as cheap
boosts, depending on how "pure" you want the self-improvement story to stay.
