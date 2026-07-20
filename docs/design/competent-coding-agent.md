# Competent coding agent harness

## Goal

Build the smallest exo TypeScript harness that behaves like a practical coding
agent rather than a shell demo. It should inspect an unfamiliar repository,
follow repository-local instructions, plan non-trivial work, make and verify
changes, retain deliberately saved knowledge, and acquire reusable skills.

The goal is to show how exoharness' primitives support a full-fledged coding
harness: durable convo, model bindings, sandboxes, tool-result artifacts, etc.

## Reference observations

Two reference agents informed the design:

- [Shelley](https://github.com/boldsoftware/shelley) from exe.dev, intentionally
  shell-centric. Its current toolset combines persistent and one-shot shells,
  patching, keyword search, working-directory control, browser access, and
  optional subagents. It also discovers Agent Skills (`SKILL.md`) and stores
  conversations durably.
- [OpenCode](https://github.com/anomalyco/opencode) exposes a broader dedicated
  surface: shell, read, glob, grep, edit, write, apply-patch, LSP, todo, skill,
  web fetch/search, questions, and subagents. It discovers `AGENTS.md`-style
  project instructions, injects environment context, progressively discloses
  skills, tracks session state, and compacts long histories.

We already have many of these pieces in the exo codebase – memory, skills, etc.
This is just composing them into a standalone coding agent, rather than as part
of the self-evolving agent Exo.

## Required capabilities

### 1. Reliable agent loop and history

Every model response, tool request, and tool result must be appended to the
canonical conversation event log. A response that requests tools is followed
by tool execution and another model round until the model returns normally or
the configured round budget is exhausted.

### 2. Sandboxed workspace execution

The agent needs a general shell in the conversation sandbox. It is responsible
for discovery, file inspection, edits, builds, tests, version-control status,
and language-specific tooling. Sandbox mounts and networking remain explicit
exo configuration rather than prompt-level conventions.

### 3. Repository instructions and live environment

At every model round, inject:

- the sandbox working directory;
- whether it is a Git worktree and the worktree root when available;
- platform and date;
- the root `AGENTS.md`, when present.

The prompt also tells the agent to check for more-specific `AGENTS.md` files
before editing nested paths. Full automatic hierarchical instruction injection
requires a dedicated read tool that can associate a target file with its
ancestor instructions; that is outside the MVP.

### 4. Planning and verification

Expose a conversation-scoped `todowrite` tool. Use it for work with at least
three meaningful steps, keep one item in progress, and persist the current list
as a conversation artifact so it is re-injected on every round.

(this matches behavior of exo)

The system policy distinguishes investigation, implementation, and
verification. The agent must inspect before editing, preserve unrelated user
changes, and run the narrowest relevant validation before reporting success.

### 5. Durable memory

Expose agent-scoped `remember` and `forget` tools. Memory is for explicit,
durable facts such as user preferences and stable project conventions. It is
not an automatic transcript summary and must not contain credentials, transient
task state, guesses, or details already represented by repository files.

Memory is stored as versioned agent artifacts and re-injected each round. This
makes it durable across conversations without coupling it to the sandbox
filesystem.

### 6. Progressive skills

Expose exo's Agent Skills tools: install, list, use, read supporting files, and
uninstall. Only skill names and descriptions are injected eagerly; full
instructions and supporting files are fetched on demand. This follows the
`SKILL.md` ecosystem convention used by the references while keeping prompt
growth bounded.

(this is following the standard protocol for skills)

### 7. Extensibility

When agent tool creation is enabled, expose install/uninstall tools and reload
installed agent tools on every model round. Also load user-configured library
tool modules. This allows the base harness to stay small while an agent or user
adds a typed capability for a recurring operation.

### 8. Safety and observability

The prompt must require read-before-write behavior, targeted edits, protection
of unrelated dirty-worktree changes, caution around destructive commands, and
no commits or remote pushes unless explicitly requested. Exo continues to own
the stronger controls: sandboxing, network configuration, secrets, tool-event
logging, result truncation, and full result artifacts.

Note: NOT attempting permissioning currently, will get to safety primitives later.

## MVP architecture

```text
user turn
   |
   v
competent-coding-agent-harness
   |-- prompt policy
   |-- live environment + root AGENTS.md
   |-- open todos, durable memory, skill catalog
   |-- shell + optional generated/library tools
   |-- todo + memory + skill tools
   v
runResponsesHarnessTurn
   |-- model call
   |-- canonical event append
   |-- tool execution in exo sandbox/host boundary
   `-- repeat until final response or round budget
```

State ownership is deliberately split:

| State                   | Scope             | Storage                           |
| ----------------------- | ----------------- | --------------------------------- |
| Messages and tool calls | Conversation      | Canonical exoharness events       |
| Current plan            | Conversation      | Versioned conversation artifact   |
| Durable memory          | Agent             | Versioned agent artifact          |
| Installed skills        | Agent             | Indexed versioned agent artifacts |
| Workspace files         | Sandbox/mount     | Exo sandbox provider              |
| Generated tool modules  | Harness workspace | `.exo/agent-tools`                |

## Non-goals for the MVP

- Permission prompts or command-by-command policy enforcement. These belong at
  the sandbox/tool boundary, not in prose alone.
- Background shell processes and resumable terminal sessions.
- LSP-backed navigation and diagnostics.
- Automatic context compaction beyond the model runtime and artifact-backed
  result truncation already provided by exo.
- Automatic web search. Networking remains an explicit sandbox capability; a
  reviewed web tool can be supplied as a library tool.
- Subagents. They add scheduling, isolated context, budgets, and result-merging
  policy and should be designed as a separate feature.
- Git commits, pushes, or pull requests. Those are user-directed workflow
  actions, not default completion criteria.

## Potential next steps to try

1. Add first-class sandbox-native `read`, `grep`, `glob`, and `apply_patch`
   tools with shared path validation and structured truncation.
2. Make the read tool inject nested instruction files exactly once when it
   crosses into a more-specific directory.
3. Add an approval/policy layer for destructive commands, external side
   effects, and writes outside the workspace.
4. Add resumable process sessions for dev servers and long-running tests.
5. Add optional web/MCP tools and an LSP tool through reviewed library modules.
6. Design context compaction with a durable handoff summary and explicit
   preservation of open todos, instruction provenance, and changed-file state.
7. Evaluate a bounded read-only explorer subagent before allowing general
   recursive delegation.
