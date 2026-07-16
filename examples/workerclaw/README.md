# WorkerClaw

WorkerClaw is an autonomous exo harness for jobs that need planning, execution,
and a durable record of progress. The agent breaks work into a task tree,
updates status as it goes, runs shell and sandbox commands, talks to external
channels through adapters when configured, and reports deliverables when
something is ready to hand off.

WorkerClaw is built on the same exo substrate as other harness examples: agents,
conversations, artifacts, adapters, and optional scheduling all live under
your exo `--root` (typically `.exo`).

## Quickstart

**Prerequisites:** a built `exo` binary, Node with pnpm, and a model API key
(`ANTHROPIC_API_KEY` and/or `OPENAI_API_KEY`).

From the exo repository root:

1. Install dependencies and configure your model key:

```bash
pnpm install
cp .env.example .env   # then fill in the provider key your model binding uses
```

2. Build the CLI:

```bash
cargo build -p exo
```

3. Register a secret + model, then create a WorkerClaw agent and conversation:

```bash
EXO=./target/debug/exo

$EXO --root .exo secret set anthropic --env ANTHROPIC_API_KEY
# or: $EXO --root .exo secret set openai --env OPENAI_API_KEY

$EXO --root .exo model register claude-sonnet-4-6 --secret anthropic
# or: $EXO --root .exo model register gpt-5.4 --secret openai

$EXO --harness typescript --root .exo \
  agent create "WorkerClaw" \
  --slug worker \
  --module examples/workerclaw/harness.ts \
  --model claude-sonnet-4-6 \
  --networking enabled

$EXO --harness typescript --root .exo \
  conversation create worker \
  --slug job-1 \
  --name "First job"
```

4. Send a task:

```bash
$EXO --harness typescript --root .exo \
  conversation send worker job-1 \
  "Plan and build a small CLI that converts CSV to JSON. Report the result when done."
```

5. Open an interactive REPL on the same conversation (optional):

```bash
$EXO --harness typescript --root .exo repl worker job-1
```

WorkerClaw will call `task_tree_init` early, keep the tree updated as it
works, use `report_deliverable` for outputs, and finish with `complete_task`.

## How it works

Each user message starts a **turn**. Flow:

1. `harness.ts` registers tools and builds developer instructions (identity,
   operating rules, optional local profile).
2. `turn-loop.ts` materializes conversation history, calls the model, executes
   tools, and continues until the task tree is finished or budgets are exhausted.
3. Events (messages, tool calls, tool results, artifact writes) append to the
   conversation log under the exo root.

**Turn-loop behavior that matters in practice:**

- If the model replies with text only before `complete_task`, WorkerClaw sends
  a developer **nudge** (default up to 3; see `WORKERCLAW_MAX_TEXT_ONLY_NUDGES`).
- Round-trip budget can be extended a few times when the task tree is still
  unfinished (`DEFAULT_ROUND_BUDGET_EXTENSIONS` in `task-tree-snapshot.ts`).
- Task-tree tool args are unwrapped via `tool-args.ts` so nested
  `{ type: "valid", value: … }` envelopes from the runtime still work.

Task-tree tools also persist a **`task-tree.json`** conversation artifact
(`task-tree-snapshot.ts`). That snapshot survives across turns so you can resume
long jobs. Successful task-tree tool results include a structured `bridgeEvent`
field — useful if a host process outside exo wants to mirror progress into its
own database or UI.

## Codebase layout

```text
examples/workerclaw/
  harness.ts                 Entry point: prompts + tool registration
  turn-loop.ts               Model/tool round loop + budget extensions
  turn-loop-nudge.ts         Text-only nudge helpers
  message-materialize.ts     Conversation history → model messages
  prompts/me.md              Committed identity and operating rules
  task-tree-tools.ts         Task tree + deliverable + complete_task tools
  task-tree-snapshot.ts      task-tree.json artifact read/write + finish checks
  tool-args.ts               Unwrap nested harness tool-arg envelopes
  introspection-tools.ts     list_adapter_events, list_conversation_events
  sandbox-tools.ts           Snapshot and rewind for the agent sandbox
  scheduler-tools.ts         Recurring tasks (optional; see env below)
  host-tools.ts              Bridge from TypeScript tool defs to Rust host tools
  guardian-tools.ts          Host guardian / self-control helpers
  adapters/                  Sidecar workers (IRC, Discord, WhatsApp, Signal, …)
  scheduler-runner/          Host process that fires scheduled sandbox tasks
  scripts/                   Local control helpers
  SELF.md                    Map of important paths for self-inspection
```

Rust adapter runtime and model-facing adapter tools live outside this folder:

- `crates/executor/src/adapter/` — adapter store, worker supervision, outbox
- `typescript/harness/adapter-tools.ts` — `create_adapter`, `send_adapter_message`, …

See [`SELF.md`](./SELF.md) for the full path map the agent reads at runtime.

## Task tree

WorkerClaw owns its own plan. Conventions:

| Depth | Role                         |
| ----- | ---------------------------- |
| 1     | Objectives                   |
| 2     | Sub-objectives               |
| 3     | TODO leaves (`isLeaf: true`) |

Status flow: `pending` → `in_progress` → `completed` or `failed`.

**Tools:**

- `task_tree_init` — declare the full tree once you understand the job
- `task_tree_upsert_node` — add or revise a single node later
- `task_tree_update_status` — move a node through statuses
- `report_deliverable` — record a URL, file, image, or text output
- `complete_task` — signal the whole job is finished (once)

**Bridge events:** successful task-tree tool results look like:

```json
{
  "ok": true,
  "bridgeEvent": {
    "type": "task_tree.init",
    "rootRef": "root",
    "nodes": []
  }
}
```

Event types include `task_tree.init`, `task_tree.upsert_node`,
`task_tree.update_status`, `deliverable.report`, and `task.complete`. A host
integration can subscribe to exo conversation events and react to these payloads
without changing the harness.

## Tools

WorkerClaw registers tools in layers (`harness.ts`):

**Built-in** (from exo harness defaults when enabled on the agent):

- `shell` — run commands in the agent sandbox
- `install_agent_tool` / `uninstall_agent_tool` when agent tool creation is enabled

**Task tree** — see above.

**Adapters:**

- `create_adapter`, `list_adapters`, `disable_adapter`, `delete_adapter`
- `send_adapter_message`

**Introspection:**

- `list_adapter_events` — adapter telemetry (connect, disconnect, inbound, errors)
- `list_conversation_events` — read the durable conversation event log

**Sandbox:**

- `list_sandbox_snapshots`, `snapshot_sandbox`, `rewind_sandbox`

**Scheduler** (when `WORKERCLAW_ENABLE_SCHEDULER=true`):

- `schedule_sandbox_task`, `list_scheduled_tasks`, `cancel_scheduled_task`, `delete_scheduled_task`

**Host-injected modules:** anything registered on the agent with
`--tool-module` / `toolModulePaths` (extra sandboxes, HTTP clients, custom
packages). Olivia hosts often inject native catalog tools here. Register at
agent create/update time:

```bash
$EXO --harness typescript agent update worker \
  --tool-module /path/to/my-tools.ts
```

## Adapters

Adapters are long-running host processes that connect WorkerClaw to external
apps (chat, IRC, CLI bridges). They are separate from one-shot sandbox commands:
adapters keep connections open, parse inbound traffic, write event history, and
wake the conversation when something needs a reply.

Shipped adapter workers live under `adapters/`:

| Adapter                                         | Notes                                          |
| ----------------------------------------------- | ---------------------------------------------- |
| [Discord](./adapters/discord/README.md)         | Bot token, rich attachments, optional voice    |
| [IRC](./adapters/irc/README.md)                 | TLS/plain TCP, mention or all-messages trigger |
| [WhatsApp](./adapters/whatsapp/setup-prompt.md) | Twilio outbound (see setup prompt)             |
| [Signal](./adapters/signal/README.md)           | `signal-cli` linked device                     |
| [Slack](./adapters/slack/README.md)             | Slack Bolt worker                              |
| [agent-cli](./adapters/agent-cli/README.md)     | Unix-socket shell bridge from any directory    |

To list configured adapters after setup:

```bash
$EXO --harness typescript adapters list
```

Adapter setup usually means creating a library adapter through the agent
(`create_adapter`) or running a local control script that sends the setup
prompts in `adapters/*/setup-prompt.md`. Each adapter README documents its
secrets and config JSON.

## Identity and local profile

`prompts/me.md` is the committed WorkerClaw identity — keep it generic.

For machine-specific instructions (your name, repo paths, style preferences),
create a local profile file. The harness loads it when present:

```text
.exo/workerclaw-profile.md
```

This file is git-ignored by convention. Override the path with
`WORKERCLAW_LOCAL_PROMPT_FILE`.

## Self-inspection

When the repo is mounted into the sandbox (for example at `/workspace/exo`),
WorkerClaw can read its own source. The self map at
`examples/workerclaw/SELF.md` points to harness code, adapter workers, and
executor modules. Set `WORKERCLAW_REPO` to the mount path and
`WORKERCLAW_SELF_MAP` to the map file if your layout differs.

## Testing

### Unit tests

WorkerClaw unit tests live next to the modules they cover and run with the
repo-wide Vitest suite:

```bash
# All exo TypeScript tests
pnpm test

# WorkerClaw-only
pnpm test examples/workerclaw
```

Covered areas include message materialization, task-tree snapshots, tool-arg
unwrapping, and text-only nudge helpers.

### Live E2E

`pnpm e2e:workerclaw` runs `scripts/workerclaw-e2e.ts` — a live check against a
real `exo` binary and model provider (same style as `pnpm e2e:agent-harnesses`,
but for WorkerClaw).

It:

1. Builds `target/debug/exo` if needed
2. Creates a temp exo root, registers a secret/model, and creates a WorkerClaw agent
   with `local-process` sandbox (no Docker/E2B required for this smoke)
3. Sends a constrained user message that must call `task_tree_init` then
   `complete_task`
4. If init lands without complete, sends one follow-up nudge
5. Asserts both tools appear in `conversation events`, and that `complete_task`
   returned a successful-looking result

```bash
pnpm e2e:workerclaw

# useful options
pnpm e2e:workerclaw -- --keep-root
pnpm e2e:workerclaw -- --model claude-sonnet-4-6
pnpm e2e:workerclaw -- --timeout-ms 300000
pnpm e2e:workerclaw -- --help
```

Requires `ANTHROPIC_API_KEY` or `OPENAI_API_KEY` in `.env`. This is a **live**
test (costs tokens); it is not part of `pnpm check`.

## Environment

| Variable                          | Purpose                                                                      |
| --------------------------------- | ---------------------------------------------------------------------------- |
| `WORKERCLAW_REPO`                 | Sandbox mount path to this repo (default `/workspace/exo`)                   |
| `WORKERCLAW_SELF_MAP`             | Path to `SELF.md` inside the mount                                           |
| `WORKERCLAW_LOCAL_PROMPT_FILE`    | Optional local profile (default `.exo/workerclaw-profile.md`)                |
| `WORKERCLAW_ENABLE_SCHEDULER`     | Set to `true` to register scheduler tools                                    |
| `WORKERCLAW_MAX_TEXT_ONLY_NUDGES` | Max developer nudges on text-only exits before `complete_task` (default `3`) |
| `WORKERCLAW_E2E_MODEL`            | Optional model override for `pnpm e2e:workerclaw`                            |
| `EXO_BIN`                         | Optional path to an `exo` binary for the E2E script                          |

Deployment-specific secrets (API keys, Twilio, OAuth tokens) belong in exo
secrets or conversation secrets — not in this tree. Use `exo secret set` or
your host's secret sync before starting adapters or injected tool modules.

## Further reading

- [`SELF.md`](./SELF.md) — path map for changing WorkerClaw itself
- [`adapter-architecture.md`](./adapter-architecture.md) — adapter store, runtime, and worker protocol
- [`docs/SELF-CONTROL.md`](./docs/SELF-CONTROL.md) — durable state, introspection, and service lifecycle
- [`scripts/workerclaw-e2e.ts`](../../scripts/workerclaw-e2e.ts) — live E2E implementation
