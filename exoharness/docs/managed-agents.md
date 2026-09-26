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
`:rw` is supplied. Mounts and `--sandbox` / `--sandbox-image` are thread settings,
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
harness paths are resolved relative to the Markdown file for local execution,
and relative to the provider's working directory for remote execution. Modules
must be installed on the provider; the spec does not bundle their code.

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

The client uses Streamable HTTP, initializes each server, and discovers its tools
when the CLI starts. All tools are available unless the server entry specifies
`allowed_tools` or `blocked_tools`, using the original tool names. An empty
`allowed_tools: []` exposes no tools. If both lists are set, blocked tools are
removed from the allowed set. Unknown tool names are errors.

Tools run without an approval prompt. Names are scoped to their
server, such as `exo_mcp__deepwiki__read_wiki_structure`. Thread events also record
the mapping to the original server and tool names.

Codex uses the pinned 0.153.4 app-server inside the sandbox. It resumes its native
thread after a restart when that state is available. Otherwise, it replays Exo's
events with structured tool calls and results, compacting between batches instead
of truncating history. The Markdown body supplies developer instructions while preserving Codex's built-in instructions.

## Vaults

For authenticated MCP servers, save a credential once and select its vault when
starting a thread. For example, with `GITHUB_TOKEN` already set:

```bash
exo vault create personal
exo vault secret create personal github \
  --mcp-server-url https://api.githubcopilot.com/mcp/ \
  --token-env GITHUB_TOKEN
unset GITHUB_TOKEN

exo chat --agent-file exoharness/examples/managed-agents/github-analyst.md \
  --vault personal
```

`--token-env` reads a variable from the process environment or `--env-file` once.
It accepts a variable name, not a token. Subsequent chats don't need that variable.
The credential URL must match the agent's MCP URL, including its path, trailing slash, and query; names are labels. A vault allows one credential per destination. Servers without a matching
credential connect unauthenticated.

```bash
exo vault list
exo vault get personal
exo vault secret list personal
exo vault secret get personal github
exo vault secret update personal github --token-env NEW_GITHUB_TOKEN
exo vault secret delete personal github
```

List/get return metadata only. Update preserves the credential id and increments
its revision. Running MCP clients check the credential before tool calls and
reconnect after rotation. Removing a credential makes later calls fail, even if
a new credential is added with the same name or URL. An in-flight call can finish.
`exo vault delete personal` deletes the vault and its credentials.

Vault access composes from global to agent to thread. Agent and thread records
store their attached vault ids. Creating a named vault doesn't grant it to every
agent. `--vault personal` attaches that vault to a new thread; repeat `--vault`
to attach more than one. Later attachments take precedence when selecting an MCP
credential for the same destination.

Bindings identify both the vault and secret. A personal MCP credential cannot
shadow a model credential with the same name. Thread attachments and selected MCP
secret references survive resume and fork:

```bash
exo chat --agent support --vault personal
exo chat --agent support --thread <thread-slug>
```

Changing the attachments or MCP destinations requires a new thread. Revoked secrets
fail instead of switching to another account. Temporary chats retain vault handles;
they do not copy secret values into their agent or thread records.

Model registration and the existing `exo secret` commands default to the global vault:

```bash
exo vault secret create global openai --token-env OPENAI_API_KEY
exo model create gpt-5.6-sol --secret openai
```

All local secrets live in the harness's encrypted vault store under
`<root>/exoharness/vaults`. The master key uses the configured Exo key provider.
On first open, existing global secrets move into the global vault. Existing agent
and thread secrets move into vaults attached at their original scopes. Secret ids
are preserved, and bindings are rewritten to include the vault id. Source files
remain until those changes are saved, so an interrupted migration can be retried.

Vault-backed chat requires an isolated sandbox. Local-process execution and mounts
that expose the vault store or its key are rejected. Harness implementations remain
trusted code.

`VaultContext` provides lookup and listing on the harness, agent, and thread.
`ExoHarness` also creates and deletes vaults. `ResourceScope` is shared with
sandboxes; vaults have global, agent, and thread contexts. `VaultHandle` owns
`list_secrets`, `put_secret`, `get_secret`, `update_secret`, and `delete_secret`.
`SecretMetadata` includes an optional destination and a revision. The MCP client
uses `VaultHandle::resolve_secret` to check the destination and read the current
value together.

## Runtime providers

The same CLI commands drive local Exo and an authenticated HTTP runtime:

```sh
exo provider create local --local-root .exo
exo provider create oss --url http://localhost:8080/exo --api-key-env EXO_RUNTIME_TOKEN
exo provider switch oss
exo agent create analyst --file ./analyst.md
exo run --agent analyst "Summarize this project"
exo thread list analyst
exo chat --agent analyst --thread <thread-slug>
```

`exo provider switch <name>` persists the selection globally for future commands.
Add `--local` to apply it to the current directory and its descendants.
`exo --provider <name> ...` overrides selection for one command. Saved agent and
thread aliases retain their provider, endpoint (including scope), and account;
switching providers never redirects an alias. Endpoint or account changes require restoring
the original connection or explicitly using a remote ID.
See [Provider configuration](../../docs/providers.md) for context and selection.

The CLI sends the Markdown spec unchanged to the selected HTTP provider. The
provider selects the harness, connects MCP servers, and resolves model bindings
and vault credentials using its own installation and state. Local Exo uses the
same setup. The OSS service supports native and TypeScript harnesses, including
the named presets, with separate MCP connections and credential selections for
each thread. Client credentials and harness modules are not copied to the server.

`type: provider` asks the selected runtime to resolve its built-in MCP; `name`
only sets its local tool namespace and grants no vault access.

HTTP `--agent-file` runs use isolated in-memory state. The CLI renews their
90-second lease every 30 seconds and deletes them on exit. Abandoned agents are
reaped every 30 seconds after expiry; cleanup cancels their execution without
stopping saved agents. Saved HTTP turns continue after the CLI disconnects;
reopening their thread recovers durable history, not missed streaming previews.

The workflow tests launch the real CLI and OSS HTTP service with a local model
fixture, so they need no external deployment or model credentials:

```sh
cargo test -p exo --test provider_workflow --test vault_oauth
cargo test -p exo-mcp --test http deepwiki_live -- --ignored
```

The second command separately tests anonymous access to the public DeepWiki MCP.
