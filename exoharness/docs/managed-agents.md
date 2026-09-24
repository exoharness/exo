# Managed agents

Write an agent in Markdown, run it locally, and talk to it on the CLI.
Agents use Exo's local state and sandbox providers.

```markdown
---
name: support-analyst
harness: codex
config:
  model: gpt-5.6-sol
---

You investigate support tickets and identify recurring product problems.
Read the available evidence, cite the tickets behind each finding, and
write your report to a file.
```

`name`, `harness`, and `config.model` are required. The body supplies the agent's
instructions. Unsupported fields are rejected so a typo or an unimplemented
feature doesn't silently change how the agent runs.

## Setup

From this checkout:

```bash
pnpm install --frozen-lockfile
cargo build -p exo
export PATH="$PWD/target/debug:$PATH"

exo secret create openai --env OPENAI_API_KEY
exo model create gpt-5.6-sol --secret openai
```

`OPENAI_API_KEY` must already be set. `--env` takes the variable's name.
The model in the file names an Exo model binding, so it can also point to a
compatible endpoint using `exo model create --base-url`. If that model isn't
registered, Exo uses the first registered model and prints which one it selected.
An explicit `--model` must match a registered binding. With no registered models,
Exo stops and prints the setup commands.

For Codex, build the sandbox image with Apple container on a Mac:

```bash
container build --platform linux/arm64 -t exo-codex-sandbox:latest \
  exoharness/containers/codex-sandbox
```

On Linux, use `docker build -t exo-codex-sandbox:latest` with the same directory.
Exo defaults to Apple container on macOS and Docker on Linux.
See [coding agent harnesses](coding-agent-harnesses.md) for other harness setup.

## Run it

`exo chat` runs inline and leaves the conversation in your terminal scrollback.
Use `--tui` to opt into the full-screen interface. Type `/help` for commands or
`/exit` to quit.

Your turn streams as it runs. Updates from other clients appear when you submit
the next line; pressing Enter on an empty prompt also checks for updates.

```bash
exo chat --agent-file exoharness/examples/managed-agents/support-analyst.md

exo run --agent-file exoharness/examples/managed-agents/support-analyst.md \
  --mount ./tickets:/workspace/tickets:ro \
  --mount ./reports:/workspace/reports:rw \
  "Analyze the tickets in /workspace/tickets and save /workspace/reports/report.md"
```

Create the host directories before mounting them. Mounts are read-only unless
`:rw` is supplied. Mounts and `--provider` / `--sandbox-image` are thread settings,
so the same agent can run in different environments.

To save an agent under a name:

```bash
exo agent create support --file exoharness/examples/managed-agents/support-analyst.md
exo agent list
exo chat --agent support
exo thread list support
exo chat --agent support --thread <thread-slug>
```

The CLI prints the agent and thread ids. Both ids and slugs work when resuming.
A missing `--thread` starts a new thread; an unknown thread is an error.
`exo run` accepts the same `--agent` and `--thread` options.

Each `--agent-file` invocation runs with an in-memory agent and thread. Its definition,
history, and runtime artifacts are discarded on exit and never appear in the saved
agent list. Registered models and secrets are available to temporary runs. Files
written to mounted host directories remain on disk.

Use `exo agent create` to save an agent and `exo chat --agent` for durable threads.
Named creation rejects an existing name. Editing or deleting the source Markdown
has no effect on saved agents. `--model` overrides the model for a thread;
`--harness` can override the harness when loading a file. Saved agents keep their
harness.

Use `harness: basic` for Exo's native tool loop, or `harness: claude-code` with an
Anthropic model binding and the Claude Code sandbox image. Custom TypeScript
harness paths are resolved relative to the Markdown file. Those modules must
remain installed; Exo saves the Markdown, not a bundle of code.

## MCP

Hosts can resolve MCP endpoints from their authenticated context:

```yaml
mcp_servers:
  - type: provider
    name: tickets
```

The host implements `exo_managed_agents::mcp::McpServerResolver` and calls
`AgentDefinition::resolve_mcp_servers` before selecting credentials and connecting.
The resolver receives each server's `name` and returns its MCP URL. The name also
sets the local tool namespace, and filters stay in the agent definition. Resolved URLs
receive the same validation as explicit URLs.
Provider resolution does not supply credentials or authorize access to them.
The standalone Exo CLI supports explicit URLs and rejects unconfigured providers.

Add remote MCP servers to the agent file:

```yaml
mcp_servers:
  - type: url
    name: deepwiki
    url: https://mcp.deepwiki.com/mcp
```

DeepWiki doesn't need credentials. Try the example:

```bash
exo run --agent-file exoharness/examples/managed-agents/repo-analyst.md \
  "Use DeepWiki to explain how tokio-rs/tokio schedules tasks."
```

For authenticated servers, use `--mcp-token-env SERVER=ENV_VAR`, for example
`exo chat --agent-file agent.md --mcp-token-env tickets=TICKETS_TOKEN`.
Exo reads the named variable from `--env-file` or the shell environment. The token
stays in the host MCP client for this CLI session; it is not saved in the agent
or forwarded to the TypeScript harness process.

The client uses Streamable HTTP, initializes each server, and discovers its tools
when the CLI starts. All tools are available unless the server entry specifies
`allowed_tools` or `blocked_tools`, using the original tool names. An empty
`allowed_tools: []` exposes no tools. If both lists are set, blocked tools are
removed from the allowed set. Unknown tool names are errors.

Tools run without an approval prompt. Names are scoped to their
server, such as `exo_mcp__deepwiki__read_wiki_structure`. Thread events also record
the mapping to the original server and tool names.

## State

State defaults to `.exo/exoharness` under the current directory. Use an absolute
`--root` to share it across working directories:

```bash
exo --root ~/.exo-managed chat --agent support --thread <thread-slug>
```

Use the same root for setup, agent creation, and chat. Agent records, resolved
configuration, the original Markdown (`managed-agents/agent.md`), and thread events
live in Exo's storage. Resume loads them from there. The existing `conversation`
commands still work for inspecting events and managing threads.

## Runtime

`executor::harness::Harness` provides the execution interface: submit a turn,
cancel it, and receive events through a sink. Submission returns when accepted.
`TurnFinished` reports the result; `ExecutionStopped` releases the thread for its
next turn. Dropping a response stream cancels execution, and CLI shutdown waits
for cleanup and the final thread events to be saved.

The existing Basic, RLM, and TypeScript executors run through an adapter to this
interface. Agent and thread storage still live behind Exo's existing storage
facade (`executor::Harness`). The execution interface doesn't choose a backend.

Codex uses the pinned 0.153.4 app-server inside the sandbox. It resumes its native
thread after a restart when that state is available. Otherwise, it replays Exo's
events with structured tool calls and results, compacting between batches instead
of truncating history. The Markdown body supplies developer instructions while preserving Codex's built-in instructions.
