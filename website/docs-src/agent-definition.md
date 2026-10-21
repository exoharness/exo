---
title: Agent definition
description: Agent definitions, threads, environments, credentials, and hosting.
---

# Agent definition

An **agent** is a configurable bundle of system instructions, [Model Context Protocol (MCP)](https://modelcontextprotocol.io/) servers, custom tools,
resources (e.g. git repositories), and defaults (harness, model).

A **thread** is an instance of that agent, containing its conversation history, execution state, filesystem state, and configuration
(environment, vaults, and overrides from the default agent settings).

A **session** represents a client’s interaction with an agent within a thread and can span multiple **turns**. A turn begins
with submitted input and includes the agent’s work in response. A thread or even a turn can continue across multiple sessions.

Each thread runs in an **environment** (sandbox and network configuration) and has access to selected **vaults** (named collections
of secrets). The environment can restrict which hosts and ports the sandbox can reach, and each secret can restrict which origins
or URLs it can be used with.

## Defining an agent

This configuration is just data that is commonly embedded in the frontmatter of a markdown file. For example, `support-analyst.md`:

```markdown
---
harness: codex
model:
  name: gpt-6.1-sol
  credential: openai
---

For each support ticket, determine whether it describes a technical
issue. If it does, try to reproduce it, explain what you found, and
suggest a workaround.
```

The following sections walk through the various ways to configure an agent, which belong in its frontmatter. The text below
that contains system instructions sent to the agent.

### Harness and model

A **harness** accepts messages and produces events, deciding how to call the model, use tools, manage context, etc. Exo supports
Codex (`codex`), Claude Code (`claude-code`), and Pi (`pi`), as well as custom harnesses. A custom harness can be selected by its
TypeScript module path, e.g. `harness: ./my-harness.ts`.

The `model` object supplies the model name and its configuration. `credential` names a secret in one of the thread's vaults.
You can also set `base_url` for a custom model endpoint. Codex supports
`reasoning_effort`, e.g. `high` or `max`. The model and its settings need to be supported by the harness you choose.

::: info Planned
`model.max_output_tokens` is parsed, but the bundled coding harnesses do not yet honor it.
:::

### MCP servers

One of the key design principles of Exo is that you don't write code _inside_ the agent implementation, but _around_ it. The
MCP integration allows you to do just that by providing tools that can be accessed remotely.

To connect an MCP server, add it to the frontmatter:

```yaml
mcp_servers:
  - type: url
    name: helpdesk
    url: https://helpdesk.example.com/mcp
```

Exo connects to the server and exposes its tools to the harness. It automatically uses vault credentials that are mapped to
the MCP's URL.

You can control which tools are exposed and which require approval.

```yaml
mcp_servers:
  - type: url
    name: helpdesk
    url: https://helpdesk.example.com/mcp
    allowed_tools: [get_ticket, update_ticket]
    permission_policy: {type: always_allow}
    tool_policies:
      update_ticket: {type: always_ask}
```

By default, MCP tools require approval. Here, the agent can `get_ticket` without asking, but needs approval for `update_ticket`.
You can also use `blocked_tools` to exclude specific tools.

### Custom tools

::: info Planned
Tool declarations in frontmatter and environment implementations are being implemented. Today the frontmatter `tools`
field accepts TypeScript module paths, and HTTP clients pass declarations as `frontend_tools` on turn requests.
:::

Custom tools let you implement behavior in your client code or in the agent's environment.

To add one, declare it in the agent's frontmatter:

```yaml
tools:
  - name: ask_user
    description: Ask the user a question and return their answer.
    parameters:
      type: object
      properties:
        question: {type: string}
      required: [question]
      additionalProperties: false
```

By default, a custom tool call will be sent through the agent's event stream, and it's up to the client to execute the tool and
provide a result. This allows you to implement features like pushing a button in a UI or soliciting some interactive feedback
from a user. Alternatively, you can implement the tool in the agent's [environment](#tool-implementations), in which case it
will automatically run there.

### Resources

Resources give the agent access to files, e.g. a git repository or a local directory:

```yaml
resources:
  - name: autoevals
    type: git
    url: https://github.com/braintrustdata/autoevals
  - name: fixtures
    type: directory
    path: ./fixtures
    mount_path: /workspace/fixtures
    mode: ro
```

`mount_path` is where the resource appears inside the sandbox. A git repository defaults to `/workspace/<resource name>`. Each thread
gets its own isolated copy, so edits are localized to the thread. Resources are writable by default, so `mode: ro` makes it read-only.

For a git URL, a new thread starts from the repository's default branch. You can set `checkout` to a branch name, tag, or
full commit ID, e.g. `checkout: main`. Exo resolves it when creating the thread, caches the source, and uses copy-on-write storage
for thread copies. Resuming a thread keeps its existing checkout, including any changes the agent made.

For private repositories, set `credential: github` on the resource and attach a vault containing that credential. The
credential needs permission to access the git server's origin, e.g. `https://github.com`.

Local paths resolve relative to the agent file and are captured when the agent is created or updated. Existing threads keep
their copies. With a remote provider, local paths refer to files on the provider's host.

### Adapters

::: info Under construction
Slack is implemented for local servers. The other adapters and hosted deployment are planned.
:::

Adapters connect an agent to chat providers like Slack, WhatsApp, and Discord. Configure them in the agent's frontmatter:

```yaml
adapters:
  - type: slack
    vault: support-slack
    allowed_channels: [C123456]
```

`name` is optional and defaults to the adapter's type; set it to tell apart two adapters of the same type.
Slack and Discord require an explicit credential vault. WhatsApp and Signal use linked-device profiles; IRC can connect
without a credential. Adapter credentials are kept separate from the vaults attached to agent threads.

Local adapters run in `--serve` mode; hosted providers manage them on the server. Each chat conversation maps to a persistent Exo thread, and the agent's response is delivered
back automatically. Slack supports Socket Mode (the default, with no public endpoint) or HTTP event delivery.
Braintrust and Cloudflare providers require HTTP. Allowed Slack users can use shared credentials from `global`;
shared identity/vault ownership adds private credentials across adapters, with optional workspace/user-ID pins.
Set `use_sender_vault: true` to use the submitter's associated vault for each Slack turn; it defaults to `false`, using
only `global`. History and files stay shared within the conversation, including data fetched using personal credentials.
See [Adapters](adapters/) for configuration and setup instructions for each type.

## Saved agents

Save a definition, then run it by its slug:

```bash
exo agent create support-analyst.md
exo run support-analyst
```

The agent's name comes from `--name`, an optional frontmatter `name:`, or the filename (without `.md`), in that order.
Exo derives its slug from that name; `--slug` sets it explicitly. `run`, `create`, and `update` use the same rules.
For example, `exo run support-analyst.md` saves the agent as `support-analyst`, so you can later use `exo run support-analyst`.

Without `--prompt`, this opens a REPL. With `--prompt`, it runs a single turn. You can update the saved definition or inspect it:

```bash
exo agent update support-analyst.md
exo agent get support-analyst
```

`exo run support-analyst.md` syncs the definition from the file before opening a thread. This is convenient
while editing the definition, and is what we use in the tutorial.
`create` requires a new slug, while `update` requires an existing agent and preserves its ID and threads.

The argument can be a Markdown file or a saved agent's name or ID. Exo treats `.md` names and paths like `./agent` as files;
other names refer to saved agents. Add `--serve` to serve the agent over HTTP instead of opening terminal chat:

```bash
exo run support-analyst.md --serve
# Or use a saved agent:
exo run support-analyst --serve
```

The local provider stores its state under `~/.exo` by default. Use `--root` to select another directory, and point the CLI and
server at the same root to share their local state.

## Threads, sessions, and turns

A thread has an id, slug (which must be unique), and display name. When you run an agent, it creates a new thread and auto-generates
these fields. If you specify a slug, it will either create a new thread with that slug name or resume the existing one.

```bash
exo run support-analyst
exo thread list support-analyst
exo run support-analyst --thread THREAD
```

Each time you connect, you interact through a session. Each submitted input starts a turn, which can include multiple model
calls, tool calls, and intermediate results before the agent finishes. A turn stays associated with the session that started it,
even if another client later follows its progress.

### What persists

Each thread's history is an append-only event log including messages, tool requests, results, and lifecycle events (e.g. start and
end of a turn). Many of the operations that Exo provides, like durable execution and policy enforcement, work by querying this
event log to reconstruct state.

Threads can also store **artifacts**: named, versioned blobs, e.g. memories or reports. These are stored by Exo
and can be retrieved independently of the sandbox's files.

A sandbox normally belongs to one thread and retains its filesystem across sessions. A client disconnecting does not delete
the thread or its sandbox. See [Data model](architecture/data-model#lifecycle) for execution lifetimes and
[Sandboxes](architecture/sandboxes) for filesystem and snapshot details.

### Configuration overrides

::: info Planned
The general `overrides` object below is being implemented. Persisted harness/model selections already work through the CLI,
and the HTTP API supports individual turn options; the shared schema and updates to existing thread overrides do not yet.
:::

To override an agent's default configuration, you can provide an `overrides` object to a thread, even after it's started.
The override itself has the same schema as the agent definition with some special rules:

- Omit a field to inherit its value.
- Supply a field to replace its value in full, including objects and lists.
- Supply `[]` to clear a list.

Configuration is resolved in order: agent defaults, then thread overrides, then turn overrides. Thread overrides persist when
you resume; turn overrides apply only to that turn. For example, a thread could select a different harness and model:

```json
{
  "overrides": {
    "harness": "pi",
    "model": {"name": "gpt-6.1-sol", "credential": "openai"}
  }
}
```

The CLI lets you override the harness and model when opening or resuming a thread:

```bash
exo run support-analyst --thread THREAD \
  --harness pi --model gpt-6.1-sol
```

These choices are saved on the thread and used when you resume it again.

### Forking

You can fork a thread's event history at the latest event, or select an earlier event with `--up-to`:

```bash
exo thread fork support-analyst THREAD
exo thread fork support-analyst THREAD --up-to EVENT_ID
```

This creates another thread with the copied history.

## Environments

Agents benefit from a sandboxed environment they can work in to read files, write/run code, and accumulate scratch work.
Exo uses SmolVM by default. The bundled coding harnesses each use a development image:

- `ghcr.io/exoharness/codex-devbox`
- `ghcr.io/exoharness/claude-code-devbox`
- `ghcr.io/exoharness/pi-devbox`

These include the harness, Python, and a Node.js/TypeScript development environment. Exo pins the defaults to tested image
digests. Custom harnesses default to `ubuntu:24.04`; select an image with the dependencies they need.
Networking is unrestricted by default, while credentials retain their own destination policies.

You can use these defaults without creating an environment file. To customize the image or network policy, define an environment
in `environment.yaml`, e.g.:

```yaml
name: support-analyst-env
image: ghcr.io/exoharness/codex-devbox:latest
networking:
  type: limited
  allowed_hosts:
    - api.openai.com
    - github.com
    - api.github.com
allowed_tcp_ports: [443]
```

This allows HTTPS connections to the listed hosts. Include hosts needed by the model, MCP servers, and any dependencies the
agent downloads. `networking.type` can also be `unrestricted` or `disabled`. Omitting `allowed_tcp_ports` allows all outbound
TCP ports allowed by the host policy.

Like agents, environments can be used directly from a file, or saved by name.

```bash
exo run support-analyst --environment-file environment.yaml

exo environment create support-analyst-env --file environment.yaml
exo run support-analyst --environment support-analyst-env
```

An environment can also set `provider`, `default_workdir`, and `file_system_mounts`.
Host mounts refer to paths on the runtime host and can be read-only or writable.
Unlike resources, host mounts expose the host's files directly.

In addition to SmolVM, Exo supports a variety of local options including Apple Containers, Docker, Firecracker (with Lima
on macOS), and hosted sandboxes including AWS AgentCore, Daytona, E2B, Sprites, and Vercel. Support varies by provider.
Exo's credential substitution and proxy-based network controls currently require SmolVM or Firecracker. Firecracker and
AWS AgentCore require optional Cargo features. Snapshot support also depends on the sandbox configuration; see
[Sandbox snapshots](architecture/sandboxes#snapshots).

### Tool implementations

::: info Planned
Environment `tool_modules` and binding implementations to agent declarations are being implemented. Today TypeScript modules
are registered by custom harnesses or the agent's module-path `tools` field.
:::

An environment can supply implementations for tools declared by the agent, using TypeScript modules:

```yaml
name: support-analyst-env
tool_modules:
  - ./tools.ts
```

Each module exports a tool, or a collection of tools, using the [TypeScript tool format](tutorials/write-your-own-agent#step-2-add-a-custom-tool).
Exo matches the exported tools to the agent's declarations by name. A tool's `initialize()` method returns a handler with an
`execute(args, execution)` method, which runs when the harness calls the tool.

Module handlers run on the runtime host. They can use `execution.context` to run commands in the thread's sandbox, read its
events, or write artifacts. Relative module paths resolve from the environment file; with a remote provider, the modules need
to be available on that provider's host.

The same agent can use different implementations in different environments. Any declared tools without an environment
implementation are handled by the client.

## Vaults

A **vault** is a named collection of secrets, such as API keys and OAuth credentials. Agent definitions refer to credentials
by name; they contain no secret values. `model.credential` selects a secret from the thread's vaults:

```yaml
model:
  name: gpt-6.1-sol
  credential: openai
```

Vaults are selected independently of the agent definition, so the same agent can use different users' credentials:

```bash
exo run support-analyst --vault personal
```

See [Vaults](architecture/vaults) for creating credentials, OAuth, per-user vaults,
selection precedence, and destination policies.

## Running and hosting

### Serving over HTTP

With `support-analyst` [saved above](#saved-agents), serve the local provider:

```bash
exo run --serve --bind 127.0.0.1:8080
```

The HTTP API is available under `/exo`. In another terminal, get the saved agent's ID using `jq`:

```bash
AGENT_ID=$(curl -fsS http://127.0.0.1:8080/exo/agent \
  | jq -r '.agents[] | select(.slug == "support-analyst") | .id')
```

Create a thread:

```bash
THREAD_ID=$(curl -fsS -X POST "http://127.0.0.1:8080/exo/agent/$AGENT_ID/thread" \
  -H 'Content-Type: application/json' \
  -d '{}' | jq -r '.thread.id')
```

Submit a turn:

```bash
curl -fsS -X POST "http://127.0.0.1:8080/exo/agent/$AGENT_ID/thread/$THREAD_ID/turn" \
  -H 'Content-Type: application/json' \
  -d '{"input":{"role":"user","content":"Triage this ticket: CSV uploads return HTTP 500 after 30 seconds."}}'
```

Watch the thread's events:

```bash
curl -N "http://127.0.0.1:8080/exo/agent/$AGENT_ID/thread/$THREAD_ID/event/watch"
```

The turn request returns a receipt after the input is accepted. Watching events replays the saved history and follows new
events. To reconnect after a particular event, add `?after=EVENT_ID` to the watch URL. You can stop watching and come back later;
the turn keeps running on the server.

Post to the same `/turn` URL to continue the thread. Supply `session_id` if you want multiple turns to belong to the same session;
otherwise, the server creates a session for the submitted turn. To cancel the turn, use the receipt's `turn.id` in place of
`TURN_ID`:

```bash
curl -fsS -X POST "http://127.0.0.1:8080/exo/agent/$AGENT_ID/thread/$THREAD_ID/turn/TURN_ID/cancel"
```

When binding to a non-loopback address, configure authentication with `--auth-file`. You can also serve a single agent with
`exo run support-analyst --serve`.

### Handling client tool calls

The application handles calls to [custom tools](#custom-tools) that aren't implemented by the environment. Their declarations are
inherited from the agent definition and can be replaced with `overrides.tools` on a thread or turn.

When the harness calls one, Exo emits a `tool_requested` event and waits for the result. The application handles the call and
posts to `/exo/agent/AGENT_ID/thread/THREAD_ID/turn/TURN_ID/frontend-tool-result`:

```json
{
  "session_id": "SESSION_ID",
  "tool_call_id": "TOOL_CALL_ID",
  "result": {
    "type": "frontend_tool_success",
    "output": {"answer": "The problem started after yesterday's release."}
  }
}
```

Use the session ID from the turn receipt and the tool call ID from the event. The result is saved in the thread's history, and
the harness continues with it. The tool's implementation and any UI interaction belong to the application.

### Approvals

In the REPL, Exo prompts before a tool call requiring approval. Answer `y` to approve, `n` to deny, or `a` to allow calls to
that tool for the rest of the session.

HTTP clients watch for `agent_runtime.approval_requested` events. Use the event payload's `approval_id` and the turn's session
ID, then post to `/exo/agent/AGENT_ID/thread/THREAD_ID/turn/TURN_ID/approval-response`:

```json
{
  "session_id": "SESSION_ID",
  "approval_id": "APPROVAL_ID",
  "approved": true,
  "allow_for_tool": false
}
```

Set `approved: false` to deny. Set both `approved` and `allow_for_tool` to `true` to allow the tool for the rest of the session.
The decision is recorded in the thread's history and the harness continues.

### Providers

A **provider** executes an agent on top of an **exoharness** which manages thread state, environments, vaults, and integrations.
Exo comes with a built-in local provider, and an HTTP client for remote providers. Hosted offerings are coming soon;
you can point the CLI at your own Exo server today:

```bash
exo provider create dev --url http://127.0.0.1:8080/exo
exo provider switch dev
```

Commands like `exo agent create` will automatically run against the selected provider.

The provider hosts the execution and state, including environments and vaults. Changing providers selects a different place
to run your agents; moving existing agents and threads between providers requires transferring their state.

## Planned additions

- [ ] Simplify the HTTP API. When `exo run FILE --serve` serves a single agent, expose agent-less routes:
  `POST /exo/turn` creates a thread and submits its first input, and `/exo/thread/THREAD/...` addresses existing
  threads. Add `POST /exo/agent/AGENT/turn` for the same new-thread flow on multi-agent or hosted servers. Return
  thread and turn IDs in the existing receipt, preserving retry semantics. Accept a plain string `input` as shorthand for a user message, so
  `curl localhost:4766/exo/turn --json '{"input": "..."}'` works. For multi-agent servers, accept agent and thread slugs
  wherever IDs are accepted. Keep the `/exo` base path until a prefix change is decided.
- [ ] Port Discord, WhatsApp, Signal, and IRC onto the managed adapter path. Slack now supports inline definitions,
  Socket Mode and HTTP, persistent conversation mappings, final replies, approvals, receipt reactions, and restart recovery.
- [ ] Implement hosted adapter deployment and HTTP provisioning for Braintrust and Cloudflare; require `transport: http` there.
  See [Adapters](adapters/) for setup and the planned providers.
- [ ] Support tool declarations in agent frontmatter and inherit them during execution. `tools` currently accepts module
  paths, and client tool declarations must be supplied as `frontend_tools` on each HTTP turn request.
- [ ] Move TypeScript tool module registration and path resolution into environments, bind implementations by declared tool
  name, and route calls without an environment implementation to the client. Environments do not yet accept `tool_modules`;
  the current registry registers modules' own definitions rather than binding implementations to agent declarations.
- [ ] Load environment tool implementations in the Codex, Claude Code, and Pi harnesses. All three already expose runtime
  and client tools, but do not load the configured TypeScript tool modules; that loader is currently used by the shared
  Responses turn loop.
- [ ] Support environment tool implementations in Python and other languages, alongside TypeScript.
- [ ] Implement one partial agent definition `overrides` schema for threads and turns: persist thread overrides, apply turn
  overrides temporarily, inherit omitted fields, replace supplied fields in full, and use `[]` to clear lists. Include
  instructions, full model settings, MCP servers, tools, resources, and adapters, and refresh the affected runtime state.
  Persisted harness/model selections and individual turn options already work; the shared `overrides` object and an API
  for updating an existing thread's overrides do not.
- [ ] Honor `model.max_output_tokens` in the bundled coding harnesses where supported, and reject unsupported settings.
  It is parsed and passed to the runtime, but Codex, Claude Code, and Pi currently do not use it.
- [ ] Implement safe unfinished-turn recovery for Claude Code and Pi. They can continue conversations across sessions, but
  neither currently exports `resumeTurn` to recover an interrupted turn after a runtime restart. Codex already does.
- [ ] Support sandbox snapshot capture and restoration with fresh egress proxy bindings and mounted resource/durable filesystems.
- [ ] Skills in agent definitions
- [ ] Memory store
- [ ] Support remote outbound connections, so that you can have a local environment that receives commands from a remote agent server
- [ ] Scheduled managed-agent deployments (e.g. CMA).

<!-- TODO: Reconcile planned additions with GitHub issues before publishing. -->
