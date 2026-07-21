# WorkerClaw Self Map

WorkerClaw is an autonomous exo harness example: plan work as a task tree,
execute in sandboxes and adapters, and report deliverables. In a normal local
startup, the repository is mounted in the sandbox at:

```text
/workspace/exo
```

Use this map before changing WorkerClaw itself.

## Important Paths

- `examples/workerclaw/harness.ts`: assembles WorkerClaw's prompt and tool registry.
- `examples/workerclaw/prompts/me.md`: durable identity and operating rules.
- `examples/workerclaw/memory-tools.ts`: agent-scoped `remember` / `forget` (artifact `memory/workerclaw-memory.json`).
- `examples/workerclaw/task-tree-tools.ts`: task tree tools + `bridgeEvent` payloads in tool results.
- `examples/workerclaw/introspection-tools.ts`: adapter and conversation introspection.
- `examples/workerclaw/sandbox-tools.ts`: sandbox snapshot and rewind tools.
- `examples/workerclaw/scheduler-tools.ts`: scheduled task tools (optional via `WORKERCLAW_ENABLE_SCHEDULER`).
- `typescript/harness/skill-tools.ts`: `install_skill` / `use_skill` / `list_skills` / `uninstall_skill` (agent artifacts).
- `examples/workerclaw/adapters/`: adapter setup prompts and worker implementations.
- `typescript/harness/adapter-tools.ts`: model-visible adapter tool definitions.
- `crates/executor/src/adapter/`: Rust adapter runtime and supervision.

## Self-evolution (rung 1)

WorkerClaw can grow capability without rebuilding itself:

- **Memory** — short durable facts across jobs (`remember` / `forget`).
- **Skills** — multi-step playbooks as agent artifacts (`install_skill` / `use_skill`). Distinct from Olivia onboarding methodology skills injected in the task briefing.
- **Agent tools** — TypeScript helpers via `install_agent_tool` (when enabled).

## Task Tree

WorkerClaw owns planning. Task-tree tools persist `task-tree.json` as a
conversation artifact and return structured `bridgeEvent` objects in tool
results so an external host (if any) can mirror progress into its own store.

- Depth 1: objectives
- Depth 2: sub-objectives
- Depth 3: TODO leaves (`isLeaf: true`)
- Status flow: `pending` → `in_progress` → `completed` / `failed`

## Environment

| Variable                       | Purpose                                       |
| ------------------------------ | --------------------------------------------- |
| `WORKERCLAW_REPO`              | Sandbox mount path (default `/workspace/exo`) |
| `WORKERCLAW_SELF_MAP`          | Path to this file                             |
| `WORKERCLAW_LOCAL_PROMPT_FILE` | Optional local profile override               |
| `WORKERCLAW_ENABLE_SCHEDULER`  | `true` to register scheduler tools            |

Host deployments may inject additional tool modules via agent `toolModulePaths`
and mirror OAuth/API credentials into exo conversation secrets — that wiring lives
outside this repository.
