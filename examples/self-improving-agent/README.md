# Self-improving agent example

Exo is a persistent agent built on Exo designed to be helpful wherever
there is a task to do from a computer. It supports task scheduling, a full
sandbox where it can install its own tools and integrations, and right now
supports an interactive CLI. Slack uses the managed adapter; see
[the Slack setup guide](../../website/docs-src/adapters/slack.md).

## Quickstart

The simplest path from a fresh checkout to a running Exo.

**Prerequisites:** Docker running, a Rust toolchain, and Node with pnpm.

1. Install JS dependencies and configure your model key:

```bash
pnpm install
cp .env.example .env   # then fill in OPENAI_API_KEY
```

2. Tell Exo who you are (optional but recommended):

```bash
./examples/self-improving-agent/exo.sh setup-profile
```

This interactively asks for your name and any local instructions, and writes
them to `.exo/exo-profile.md` (git-ignored). The harness loads it as an
extra prompt on every turn, so the agent greets you by name from its first
reply. You can rerun this or edit the file directly at any time.

3. Build and start everything with one command (run from the repo root):

```bash
./examples/self-improving-agent/exo.sh fresh
```

This builds the `exo` binary, creates the agent and a `dev` conversation, pulls
the Docker sandbox image, mounts this repo at `/workspace/exo`, starts the
scheduler and managed adapter server, and drops you into a REPL.

4. Chat with Exo in the REPL:

```text
dev> hello! what can you do?
```

You can also resume the same thread from another terminal with `exo run`.

Notes:

- `fresh` deletes existing agents, conversations, and adapters first. For
  day-to-day restarts that keep state, drop `fresh`:
  `./examples/self-improving-agent/exo.sh`
- For a minimal start without Docker defaults or adapter setup:
  `./examples/self-improving-agent/exo.sh --template minimal --pull-sandbox`

## Self Introspection

Exo starts with sandbox shell support by default. The startup script mounts
this repository into the sandbox at `/workspace/exo` and makes that path
available to the harness as `EXO_REPO`. The self map lives at:

```text
/workspace/examples/self-improving-agent/examples/self-improving-agent/SELF.md
```

The checked-in source for that map is `examples/self-improving-agent/SELF.md`. It points
Exo to the harness, prompts, adapter runtime, scheduler, sandbox tools, and
service guardian. Use `--self-repo-mount <path>` or `EXO_REPO` to choose a
different sandbox mount path.

## Service Guardian

`examples/self-improving-agent/scripts/exo-service-guardian` is a host-side helper for
self-maintenance. It owns build and service-control actions that should happen
outside the agent's sandbox, while preserving `.exo` state such as adapter
pairing data, conversations, artifacts, and sandbox records.

Common commands:

```bash
examples/self-improving-agent/scripts/exo-service-guardian status
examples/self-improving-agent/scripts/exo-service-guardian build
examples/self-improving-agent/scripts/exo-service-guardian restart-adapters
examples/self-improving-agent/scripts/exo-service-guardian restart-scheduler
examples/self-improving-agent/scripts/exo-service-guardian restart-all --build
```

Save local launch settings for later restarts with:

```bash
examples/self-improving-agent/scripts/exo-service-guardian configure
```

The service guardian manages only the scheduler and adapter runners. Start or
reconnect an interactive REPL with `./examples/self-improving-agent/exo.sh`.

Exo can call `rebuild_and_restart_exo` after changing itself. That narrow tool
validates and builds Exo, then hands a restart to the guardian after a short
delay so the current agent turn can finish before services stop. Operators use
the guardian CLI commands above for status, logs, and targeted service control.
Detached restart output is written to
`.exo/exo-service-guardian-actions.log`.

When `./examples/self-improving-agent/exo.sh --control` is running, it also acts
as the foreground REPL supervisor. Guardian builds write
`.exo/exo-control.restart`; the control wrapper sees that marker, restarts only
the child `exo run`, and keeps your terminal open.

## Setting up the identity

`examples/self-improving-agent/prompts/me.md` is the committed, generic Exo identity
prompt. It's best to keep this high level, and not specific to any given
instance or the local deployment environment.

Use `.exo/exo-profile.md` for local instructions instead. The harness loads
it as an additional developer prompt when it exists, and `.exo` is ignored by
git. To create it interactively:

```bash
./examples/self-improving-agent/exo.sh setup-profile
```

The script asks for the user's name and any extra local instructions. To use a
different local prompt path, set `EXO_LOCAL_PROMPT_FILE` or pass
`--local-prompt-file <path>`.

## Tools

Exo's bootstrap profile includes:

- `shell`
- `inspect_tools`
- `manage_tool`
- `rebuild_and_restart_exo`

`manage_tool` is the only tool-management write surface. It installs or removes
workspace-local tools from workspace-relative directories or exact pinned Git
commits; an install replaces any existing entry with the same stable manifest
id. When creating a tool with sandbox `shell`, write it under
`/workspace/examples/self-improving-agent/.exo/tool-sources/<name>` and give `manage_tool` the relative
path `.exo/tool-sources/<name>`, not an absolute sandbox path.
`inspect_tools` lists or gets active and installed tools. Changes are available
on the next model round.

Installable tools live in `.exo/tools/`. Their `exo-tool.json` contains exactly
`schemaVersion`, `id`, and `module`; the TypeScript module owns the model
definition, schemas, handler, and initialization contract. Lockfile entries
contain only `id`, `source`, `initialization`, and `installPath`.

Declare tool modules in the agent spec; inspect them with `exo agent get NAME`.
Configured library modules remain supported. The legacy `install_agent_tool`,
`uninstall_agent_tool`, and `.exo/agent-tools/` paths require
`enable_agent_tool_creation: true`, which is off by default.

It also adds scheduler tools:

- `schedule_sandbox_task`
- `list_scheduled_tasks`
- `cancel_scheduled_task`
- `delete_scheduled_task`

`cancel_scheduled_task` disables a task and preserves its record/history.
`delete_scheduled_task` removes the task record and stored run history.

And skill tools (see `../exoharness/docs/design/skills.md` for the design):

- `install_skill`
- `list_skills`
- `use_skill`
- `read_skill_file`
- `uninstall_skill`

Skills follow the standard agent-skills format (a `SKILL.md` with `name` and
`description` frontmatter plus markdown instructions, optionally bundling text
files), so skills published for Claude Code, OpenClaw, or Hermes install
unchanged. They are stored as agent artifacts, persist across conversations,
and survive sandbox rewinds. Each turn the agent sees only installed skill
names and descriptions; it loads a skill's full instructions with `use_skill`
when a task matches.

## Adapters

The old self-agent adapter workers and setup prompts have been removed. Configure
Slack in an agent definition and run it with `exo run AGENT --serve`; see the
[Slack setup guide](../../website/docs-src/adapters/slack.md). The adapter submits
turns and delivers their output. It does not use scheduler wakeups or
`send_adapter_message`.

## Sandbox Modes

Exo conversations default to `sandboxScope: "agent"`. The `shell` tool uses
the sticky agent sandbox, so packages installed through the Exo REPL, such as
`curl` or `python3`, are available to scheduled task runs and future
conversations for the same agent while that warm sandbox is still alive. Normal
REPL exits leave the warm sandbox running so the next Exo process can
reattach to it.

Because exo defaults to agent scope, you don't need to specify anything from
the cli. The following command will create a REPL with the agent and a
persistent sandbox that will be durable across conversations

```bash
./examples/self-improving-agent/exo.sh --pull-sandbox
```

If you want a conversation to have its own sandbox, use `sandboxScope: "conversation"`:

```bash
./examples/self-improving-agent/exo.sh --conversation isolated-dev --sandbox-scope conversation
exo thread --harness exo update exo-agent isolated-dev --sandbox-scope conversation
```

Scheduled tasks also default to `sandboxMode: "agent"`. A task can explicitly use
`sandboxMode: "conversation"` to run in the current conversation's sandbox, or
`sandboxMode: "task_fresh"` to create a separate task-owned sandbox.

Important limitation: the current sandbox filesystem is not durable across warm
container death. Exo stores a durable pointer to the agent's sandbox, but
package installs made interactively live in the running warm container. If the
host restarts or the container backend cleans up the warm container, a later
scheduled task may recreate the sandbox from the base image and lose packages
installed with commands like `apt-get install python3`. Stale warm containers are
eligible for orphan cleanup after roughly 24 hours.

For reliable scheduled tasks, prefer one of these:

- Use a sandbox image that already contains required dependencies.
- Include a `setupCommand` that installs required packages before the task runs.
- Keep task code/data on mounted storage instead of relying on mutated container
  filesystem state.

The task-owned sandbox starts from the configured image and mounts. It is reused
across the task's runs and stopped when the task is cancelled.

The current scope model is Exo-specific policy on top of conversation-owned
exoharness sandbox records. The default mental model is agent-scoped, while
conversation and task scopes remain available for isolation.
