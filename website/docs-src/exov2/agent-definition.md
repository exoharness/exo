---
title: Agent definition
description: Agent definitions, threads, environments, credentials, and hosting.
---

# Agent definition

An **agent** is a configurable bundle of system instructions, [Model Context Protocol (MCP)](https://modelcontextprotocol.io/) servers, custom tools,
resources (eg git repositories), and defaults (harness, model).

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
TypeScript module path, eg. `harness: ./my-harness.ts`.

The `model` object supplies the model name and its configuration. `credential` names a secret in one of the thread's vaults.
You can also set `base_url` for a custom model endpoint, or `max_output_tokens` to limit the output. Codex supports
`reasoning_effort`, eg. `high` or `max`. The model and its settings need to be supported by the harness you choose.

### MCP servers

One of the key design principles of Exo is that you don't write code _inside_ tha agent implementation, but _around_ it. The
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

Custom tools are passed to the agent and once invoked, are executed outside of Exo, either in your client code or in the
agent's environment.

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

Resources give the agent access to files, eg. a git repository or a local directory:

```yaml
resources:
  - name: autoevals
    type: git_repository
    url: https://github.com/braintrustdata/autoevals
  - name: fixtures
    type: directory
    path: ./fixtures
    mount_path: /workspace/fixtures
    mode: ro
```

`mount_path` is where the resource appears inside the sandbox. A git repository defaults to `/workspace/<repo name>`. Each thread
gets its own isolated copy, so edits are localized to the thread. Resources are writable by default, so `mode: ro` makes it read-only.

For a git URL, a new thread starts from the repository's default branch. You can set `checkout` to a branch name, tag, or
commit ID, eg. `checkout: main`. Exo resolves it when creating the thread, caches the source, and uses copy-on-write storage
for thread copies. Resuming a thread keeps its existing checkout, including any changes the agent made.

For private repositories, set `credential: github` on the resource and attach a vault containing that credential. The
credential needs permission to access the git server's origin, eg. `https://github.com`.

Local paths resolve relative to the agent file and are captured when the agent is created or updated. Existing threads keep
their copies. With a remote provider, local paths refer to files on the provider's host.

### Adapters

Adapters connect an agent to chat providers like Slack, WhatsApp, and Discord. The agent definition names the adapters it uses:

```yaml
adapters: [support-slack]
```

The deployment supplies the settings for those names through `exo serve --adapters-file adapters.yaml`. This lets the same
agent definition be used in different deployments, with different accounts and channels.

## Threads, sessions, and turns

Running an agent opens a new thread unless you select an existing one. The CLI prints its slug and ID, either of which can be
used to resume it:

```bash
exo agent run --agent support-analyst
exo thread list support-analyst
exo agent run --agent support-analyst --thread THREAD
```

Each time you connect, you interact through a session. Each submitted input starts a turn, which can include multiple model
calls, tool calls, and intermediate results before the agent finishes. A turn stays associated with the session that started it,
even if another client later follows its progress.

### What persists

A thread's history is an append-only event log. It includes messages, tool requests and results, and lifecycle events like the
start and end of a turn. The harness uses that history to construct its context, so the complete event log can contain more
than what is sent to the model on a particular call.

Threads can also store **artifacts**: named, versioned blobs, eg. a report or saved execution state. These are stored by Exo
and can be retrieved independently of the sandbox's files.

Resuming a thread reuses its sandbox and resource copies. Resource files survive sandbox replacement; files written elsewhere
in the sandbox follow that sandbox backend's lifecycle. Updating an agent definition preserves the history and files in its
existing threads.

An accepted turn served over HTTP continues if the client disconnects. You can reconnect and read its events later, or explicitly
cancel it. The HTTP examples below show how to do this.

### Configuration overrides

The agent definition supplies the defaults. An `overrides` object lets a thread save configuration changes, and a
turn supply temporary overrides on top of those. Overrides use the same field names and types as the agent definition, so
this applies to the harness, model, tools, MCP servers, etc.

The rules are the same for each field:

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

The same object can override the `tools` declarations. An override doesn't change the saved agent definition or other threads.

The CLI lets you override the harness and model when opening or resuming a thread:

```bash
exo agent run --agent support-analyst --thread THREAD \
  --harness pi --model gpt-6.1-sol
```

These choices are saved on the thread and used when you resume it again. Switching harnesses keeps the same thread and its
conversation history. The model and credentials still need to work with the selected harness.

The HTTP API accepts `overrides` when creating a thread or submitting a turn. Omitted fields use the agent's current defaults,
with any saved thread overrides applied before the turn's overrides.

### Forking

You can fork a thread's event history at the latest event, or select an earlier event with `--up-to`:

```bash
exo thread fork support-analyst THREAD "Try another approach"
exo thread fork support-analyst THREAD "Try another approach" --up-to EVENT_ID
```

This creates another thread with the copied history. Filesystem restoration requires a corresponding sandbox snapshot;
selecting an earlier event only selects the history to copy. Forking threads with filesystem resources is not supported yet.
The local managed-agent API also does not currently expose an in-place rewind operation.

## Environments

Agents benefit from a sandboxed environment they can work in to read files, write/run code, and accumulate scratch work.
Exo uses SmolVM by default, with a development image for the selected harness:

- `ghcr.io/exoharness/codex-devbox`
- `ghcr.io/exoharness/claude-code-devbox`
- `ghcr.io/exoharness/pi-devbox`

These include the harness, Python, and a Node.js/TypeScript development environment. Exo pins the defaults to tested image
digests. Networking is unrestricted by default, while credentials retain their own destination policies.

You can use these defaults without creating an environment file. To customize the image or network policy, define an environment
in `environment.yaml`, eg.:

```yaml
name: support-analyst-env
config:
  image: ghcr.io/exoharness/codex-devbox:latest
  policy:
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

Use the file directly, or save it as a named environment:

```bash
exo agent run --agent support-analyst --environment-file environment.yaml

exo environment create support-analyst-env --file environment.yaml
exo agent run --agent support-analyst --environment support-analyst-env
```

An environment can also set `config.provider`, `config.default_workdir`, and `config.file_system_mounts`. Mounts refer to paths
on the runtime host and can be read-only or writable. Unlike resources, host mounts expose the host's files directly.

Exo also supports Apple Containers on macOS, Docker, Firecracker (via Lima on macOS), and sandbox backends including AWS
AgentCore, Daytona, E2B, Sprites, and Vercel. Backend capabilities differ: SmolVM and Firecracker support the credential
substitution and network controls used by managed coding agents. Filesystem resources currently work with local backends.

An environment's definition is saved with the thread. Updating a named environment affects subsequent selections of that
environment; an existing thread keeps its saved definition until you select an updated one for it.

### Tool implementations

An environment can supply implementations for tools declared by the agent, using TypeScript modules:

```yaml
name: support-analyst-env
config:
  tool_modules:
    - ./tools.ts
```

Each module exports a tool, or a collection of tools, using the [TypeScript tool format](../tutorials/write-your-own-agent#step-2-add-a-custom-tool).
Exo matches the exported tools to the agent's declarations by name. A tool's `initialize()` method returns a handler with an
`execute(args, execution)` method, which runs when the harness calls the tool.

Module handlers run on the runtime host. They can use `execution.context` to run commands in the thread's sandbox, read its
events, or write artifacts. Relative module paths resolve from the environment file; with a remote provider, the modules need
to be available on that provider's host.

The same agent can use different implementations in different environments. Any declared tools without an environment
implementation are handled by the client.

## Vaults and credentials

A **vault** is a named collection of secrets, such as API keys and OAuth credentials, stored encrypted at rest. Each secret
has a name and a policy describing where it can be used.

With the local provider, threads inherit the `global` vault, vaults attached to their agent, and any explicitly attached vaults.
When multiple vaults contain a matching credential, the more specifically attached vault takes precedence. With remote
providers, access to vaults also depends on the authenticated user.

### Creating and selecting credentials

With `OPENAI_API_KEY` set, save a model credential:

```bash
exo vault secret create global --preset openai
```

This creates a secret named `openai` with permission to access `https://api.openai.com`. `model.credential: openai` selects it.
The preset is short-hand for:

```bash
exo vault secret create global openai \
  --token-env OPENAI_API_KEY --allow-origin https://api.openai.com
```

`--token-env` takes the environment variable's name. The CLI reads its value and stores it in the vault.

You can keep credentials in separate vaults, eg. for different users or customers:

```bash
exo vault create personal
exo vault secret create personal --preset github
exo agent run --agent support-analyst --vault personal
```

The GitHub preset uses `gh` to access your login and permits credential use at `https://github.com` and `https://api.github.com`.
You can also import a token with `--token-env GITHUB_PAT`. `--vault` can be repeated, and works when resuming a thread as well.

For an MCP server that supports OAuth, you can log in using its resource URL:

```bash
exo vault secret create personal helpdesk --url https://helpdesk.example.com/mcp
```

Exo discovers the server's OAuth settings and stores the resulting credential with permission to access that URL. MCP
credentials are selected by their destination policies, so the secret does not have to share the server's name.

### Destination policies

`--allow-origin` permits a secret's use at an origin (scheme, host, and port). `--allow-url` permits its use at an exact URL.
Both can be repeated. You can update the policy without replacing the secret:

```bash
exo vault secret update personal helpdesk --allow-url https://helpdesk.example.com/mcp
```

For sandbox credentials, both the environment's network policy and the secret's destination policy need to allow the request.
Unrestricted networking still respects each secret's policy.

### Credential substitution

Agents get placeholder credentials like `OPENAI_API_KEY=exo_egress_d81f72ff9a6b4ff2bd4e1bae63c207b0`. Exo substitutes the real
credential outside of the sandbox, on requests to permitted destinations. This lets the agent use a credential without being
able to read its value. MCP connections use credentials outside the sandbox as well.

## Running and hosting

### Saved agents

Save a definition, then run it by its slug:

```bash
exo agent create support-analyst --file support-analyst.md
exo agent run --agent support-analyst
```

The creation argument supplies the display name, and Exo derives its slug from that name. You can set a different slug with
`--slug`. With `--agent-file`, the filename supplies the display name.

Without `--prompt`, this opens a REPL. With `--prompt`, it runs a single turn. You can update the saved definition or inspect it:

```bash
exo agent update support-analyst --file support-analyst.md
exo agent get support-analyst
```

`exo agent run --agent-file support-analyst.md` syncs the definition from the file before opening a thread. This is convenient
while editing the definition, and is what we use in the tutorial.

The local provider stores its state under `.exo` by default. Use `--root` to select another directory, and point the CLI and
server at the same root to share their local state.

### Serving over HTTP

To serve the local provider, run:

```bash
exo serve --bind 127.0.0.1:8080
```

The HTTP API is available under `/exo`. In another terminal, find the saved agent and create a thread (these examples use `jq`
to read IDs from the responses):

```bash
BASE=http://127.0.0.1:8080/exo
AGENT_ID=$(curl -fsS "$BASE/agent" \
  | jq -r '.agents[] | select(.slug == "support-analyst") | .id')
THREAD_ID=$(curl -fsS -X POST "$BASE/agent/$AGENT_ID/thread" \
  -H 'Content-Type: application/json' -d '{}' | jq -r '.thread.id')
```

Submit a turn, then watch the thread's events:

```bash
TURN_ID=$(curl -fsS -X POST "$BASE/agent/$AGENT_ID/thread/$THREAD_ID/turn" \
  -H 'Content-Type: application/json' \
  -d '{"input":{"role":"user","content":"Triage this ticket: CSV uploads return HTTP 500 after 30 seconds."}}' \
  | jq -r '.turn.id')
curl -N "$BASE/agent/$AGENT_ID/thread/$THREAD_ID/event/watch"
```

The turn request returns a receipt after the input is accepted. Watching events replays the saved history and follows new
events. To reconnect after a particular event, add `?after=EVENT_ID` to the watch URL. You can stop watching and come back later;
the turn keeps running on the server.

Post to the same `/turn` URL to continue the thread. Supply `session_id` if you want multiple turns to belong to the same session;
otherwise, the server creates a session for the submitted turn. To cancel the turn:

```bash
curl -fsS -X POST "$BASE/agent/$AGENT_ID/thread/$THREAD_ID/turn/$TURN_ID/cancel"
```

When binding to a non-loopback address, configure authentication with `--auth-file`. You can also serve a single agent with
`exo serve --agent support-analyst`.

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

### Providers

A **provider** executes an agent on top of an **exoharness** which manages thread state, environments, vaults, and integrations.
Exo comes with a built-in provider that you can run locally, as well as an http-based client that works with remote providers
(like Braintrust).

To use a provider, run:

```bash
exo provider create braintrust --url https://api.braintrust.dev/exo
exo provider switch braintrust
```

Commands like `exo agent create` will automatically run against the provider. You can also point the CLI at your own Exo server:

```bash
exo provider create dev --url http://127.0.0.1:8080/exo
exo provider switch dev
```

The provider hosts the execution and state, including environments and vaults. Changing providers selects a different place
to run your agents; moving existing agents and threads between providers requires transferring their state.

## Planned additions

- [ ] Accept resource `checkout` as a Git ref string (branch, tag, or commit), replacing the tagged branch/commit object.
- [ ] Support tool declarations in agent frontmatter and inherit them during execution.
- [ ] Move TypeScript tool module registration and path resolution into environments, bind implementations by declared tool
  name, and route calls without an environment implementation to the client.
- [ ] Load environment tool implementations in the Codex, Claude Code, and Pi harnesses.
- [ ] Implement one partial agent definition `overrides` schema for threads and turns: persist thread overrides, apply turn
  overrides temporarily, inherit omitted fields, replace supplied fields in full, and use `[]` to clear lists.
- [ ] Skills in agent definitions
- [ ] Memory store
- [ ] Support remote outbound connections, so that you can have a local environment that receives commands from a remote agent server
- [ ] Scheduled deployments (eg CMA).

<!-- TODO: Reconcile planned additions with GitHub issues before publishing. -->
