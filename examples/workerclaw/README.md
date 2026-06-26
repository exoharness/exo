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

**Prerequisites:** a built `exo` binary, Node with pnpm, and a model API key.

From the exo repository root:

1. Install dependencies and configure your model key:

```bash
pnpm install
cp .env.example .env   # then fill in the provider key your model binding uses
```

2. Build the CLI (Rust 1.95+):

```bash
cargo build -p exo-cli
```

3. Create a WorkerClaw agent and conversation:

```bash
EXO=./target/debug/exo

$EXO --harness typescript --root .exo \
  agent create "WorkerClaw" \
  --slug worker \
  --module examples/workerclaw/harness.ts \
  --model gpt-5.4

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

Each user message starts a **turn**. The harness in `harness.ts` assembles
prompts, registers tools, and runs the model loop until the turn completes.
Events (messages, tool calls, tool results, artifact writes) append to the
conversation log under `.exo`.

Task-tree tools also persist a **`task-tree.json`** conversation artifact.
That snapshot survives across turns so you can resume long jobs. Tool results
from task-tree tools include a structured `bridgeEvent` field — useful if a
host process outside exo wants to mirror progress into its own database or UI.

## Codebase layout

```text
examples/workerclaw/
  harness.ts              Entry point: prompts + tool registration
  prompts/me.md           Committed identity and operating rules
  task-tree-tools.ts      Task tree + deliverable + complete_task tools
  introspection-tools.ts  list_adapter_events, list_conversation_events
  sandbox-tools.ts        Snapshot and rewind for the agent sandbox
  scheduler-tools.ts      Recurring tasks (optional; see env below)
  host-tools.ts           Bridge from TypeScript tool defs to Rust host tools
  adapters/               Sidecar workers (IRC, Discord, WhatsApp, Signal, …)
  scheduler-runner/       Host process that fires scheduled sandbox tasks
  scripts/                Local control helpers (shared with other examples)
  SELF.md                 Map of important paths for self-inspection
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
- `install_agent_tool` / agent tool creation when enabled on the agent config

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
packages). Register at agent create/update time:

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
| [agent-cli](./adapters/agent-cli/README.md)     | Unix-socket shell bridge from any directory    |
| [Slack](./adapters/slack/README.md)             | Placeholder worker today                       |

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

## Environment

| Variable                       | Purpose                                                       |
| ------------------------------ | ------------------------------------------------------------- |
| `WORKERCLAW_REPO`              | Sandbox mount path to this repo (default `/workspace/exo`)    |
| `WORKERCLAW_SELF_MAP`          | Path to `SELF.md` inside the mount                            |
| `WORKERCLAW_LOCAL_PROMPT_FILE` | Optional local profile (default `.exo/workerclaw-profile.md`) |
| `WORKERCLAW_ENABLE_SCHEDULER`  | Set to `true` to register scheduler tools                     |

Deployment-specific secrets (API keys, Twilio, OAuth tokens) belong in exo
secrets or conversation secrets — not in this tree. Use `exo secret set` or
your host's secret sync before starting adapters or injected tool modules.

## Further reading

- [`SELF.md`](./SELF.md) — path map for changing WorkerClaw itself
- [`adapter-architecture.md`](./adapter-architecture.md) — adapter store, runtime, and worker protocol
- [`docs/SELF-CONTROL.md`](./docs/SELF-CONTROL.md) — durable state, introspection, and service lifecycle
