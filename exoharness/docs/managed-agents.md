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

## Command names

Resource commands use `create`, `list`, `get`, `update`, and `delete` where supported:

| Resource                                     | Commands                                    |
| -------------------------------------------- | ------------------------------------------- |
| `agent`, `thread`, `provider`, `environment` | `create`, `list`, `get`, `update`, `delete` |
| `vault`                                      | `create`, `list`, `get`, `delete`           |
| `vault secret`                               | `create`, `list`, `get`, `update`, `delete` |
| `sandbox`, `agent mount`, `thread mount`     | `create`, `list`, `delete`                  |
| `model`, `sandbox provider`                  | `create`, `list`                            |

Use `exo thread list AGENT` to list saved chats and `exo agent run --agent AGENT --thread THREAD`
to resume one. `exo sandbox stop` retains a sandbox record; `exo sandbox delete` destroys
it and deletes the record. `sandbox list` shows running sandboxes; add `--all` to include
stopped ones. Runtime-specific scope belongs in the provider URL. Exo preserves its path
and query parameters without interpreting them.

## Setup

From this checkout:

```bash
pnpm install --frozen-lockfile
cargo build -p exo
export PATH="$PWD/target/debug:$PATH"

exo vault secret create global openai --token-env OPENAI_API_KEY
exo model create gpt-5.6-sol --secret openai
```

`OPENAI_API_KEY` must already be set. `--token-env` takes the variable's name.
The model in the file names an Exo model binding, so it can also point to a
compatible endpoint using `exo model create --base-url`. If that model isn't
registered, Exo uses the first registered model and prints which one it selected.
An explicit `--model` must match a registered binding. With no registered models,
Exo stops and prints the setup commands.

For Codex, build the sandbox image with Docker:

```bash
docker build -t exo-codex-sandbox:latest \
  exoharness/containers/codex-sandbox
```

Exo defaults to SmolVM on macOS and Docker on Linux. The CLI's default `smolvm`
Cargo feature downloads and caches a checksum-verified runtime on first use when
`smolvm` is not on `PATH`; no separate SmolVM installation is needed. Explicit
`--smolvm-binary` / `SMOLVM_BIN` paths take precedence and must be valid.
Build with `cargo build -p exo --no-default-features` to require an installed
runtime instead. Library consumers opt in with `exoharness`'s `smolvm` feature.
Set `SMOLMACHINES_NO_DOWNLOAD=1` to use only an installed or already cached runtime.
See [coding agent harnesses](coding-agent-harnesses.md) for other harness setup.

## Run it

`exo agent run` runs inline and leaves the conversation in your terminal scrollback.
Use `--tui` to opt into the full-screen interface. Type `/help` for commands or
`/exit` to quit.

Your turn streams as it runs. Updates from other clients appear when you submit
the next line; pressing Enter on an empty prompt also checks for updates.

```bash
exo agent run --agent-file exoharness/examples/managed-agents/support-analyst.md

exo agent run --agent-file exoharness/examples/managed-agents/support-analyst.md \
  --mount ./tickets:/workspace/tickets:ro \
  --mount ./reports:/workspace/reports:rw \
  --prompt "Analyze the tickets in /workspace/tickets and save /workspace/reports/report.md"
```

Create the host directories before mounting them. Mounts are read-only unless
`:rw` is supplied. Mounts and `--sandbox` / `--sandbox-image` are thread settings,
so the same agent can run in different environments.

To save an agent under a name:

```bash
exo agent create support --file exoharness/examples/managed-agents/support-analyst.md
exo agent list
exo agent run --agent support
exo thread list support
exo agent run --agent support --thread <thread-slug>
```

The CLI prints the agent and thread ids. Both ids and slugs work when resuming.
A missing `--thread` starts a new thread; an unknown thread is an error.
Add `--prompt "..."` to run a single turn and exit.

Each `--agent-file` invocation creates or updates a saved agent from the Markdown
file, then starts a saved thread. The agent slug combines the filename with a hash
of its canonical absolute path; rerunning the same file reuses the agent and
replaces its saved definition. Moving the file creates a different agent. Add
`--thread <slug>` to resume a saved thread. Agents and history remain until deleted.

Use `exo agent create` to save an agent with an explicit name and `exo agent run
--agent` to run it without syncing a file. Named creation rejects an existing name.
Editing or deleting the source Markdown has no effect until it is synced again. `--model` overrides the model for a thread;
`--harness` can override the harness when loading a file. Saved agents keep their
harness.

Use `harness: basic` for Exo's native tool loop, or `harness: claude-code` with an
Anthropic model binding and the Claude Code sandbox image. Custom TypeScript
harness paths are resolved relative to the Markdown file for local execution,
and relative to the provider's working directory for remote execution. Modules
must be installed on the provider; the spec does not bundle their code.

## Specs and credentials

`exo agent update NAME --file agent.md` replaces the saved spec. It preserves
threads and restores the previous spec if the provider rejects the update.
`exo agent get NAME` shows the saved spec and resolved harness configuration.

Declare TypeScript tool modules in `tools: [./tools.ts]`; their paths follow the
same resolution rules as harness modules. Optional `config.braintrust` retains
tracing settings (`org_name` and `project: {kind: name, value: PROJECT}`). `tool_creation: true` enables the
harness's agent-authored tool support. For `harness: exo`, set `config.module`
to the Exo harness module. Sandbox configuration, including the image, belongs
in an environment definition, not the agent file.

Model bindings select the upstream model and base URL. Their credentials live
in vaults: `exo model create MODEL --vault team --secret openai`. Sandbox provider
bindings likewise accept `--vault team --secret KEY` under `exo sandbox provider`.

## Serve over HTTP

See the [remote server plan](design/remote-server.md) for personal/team login,
vault ownership, multiplayer sharing, and the web UI.

```sh
exo serve --agent support --bind 127.0.0.1:8080
# In another terminal:
exo provider create served --url http://127.0.0.1:8080/exo
exo --provider served agent run --agent support --prompt "Summarize today's tickets"
```

The server uses the existing managed-agent HTTP API: agent discovery, saved
threads, turns, event streaming, cancellation, approval responses, and reconnect.
With `--agent NAME`, only that agent is visible and other agents are inaccessible.
Omit `--agent` to serve the local provider, including agent creation. This does not expose the raw ExoHarness `/request` transport.

Declare adapter attachment names in the agent spec, for example
`adapters: [support-slack]`. Pass deployment definitions as
`exo serve --agent support --adapters-file adapters.yaml`.
Each YAML key is an attachment name; its value is an `AdapterConfig` with
`adapterType`, `workerCommand`, optional `initialization`, `stateDir`, and
`secretEnv: [{env: SLACK_BOT_TOKEN, vault: team, secretId: slack-token}]`.
Credentials are vault references, never literal values. Omitted `vault` means `global`.

The service supervises the attached workers. Restart it after changing adapter
attachments or deployment configuration; existing adapter IDs, threads, and
queued deliveries survive a restart. Removed spec attachments are disabled.

Use `--auth-file` for Google/OIDC login and `--multiplayer` to share agents and threads. Personal vaults remain private. See the [setup instructions](design/remote-server.md#google-setup). Without auth, the listener must be loopback.

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
exo agent run --agent-file exoharness/examples/managed-agents/repo-analyst.md \
  --prompt "Use DeepWiki to explain how tokio-rs/tokio schedules tasks."
```

The client uses Streamable HTTP, initializes each server, and discovers its tools
when the CLI starts. All tools are available unless the server entry specifies
`allowed_tools` or `blocked_tools`, using the original tool names. An empty
`allowed_tools: []` exposes no tools. If both lists are set, blocked tools are
removed from the allowed set. Unknown tool names are errors.

Tool policies control approval prompts. Names are scoped to their
server, such as `exo_mcp__deepwiki__read_wiki_structure`. Thread events also record
the mapping to the original server and tool names.

Codex uses the pinned 0.153.4 app-server inside the sandbox. It resumes its native
thread after a restart when that state is available. Otherwise, it replays Exo's
events with structured tool calls and results, compacting between batches instead
of truncating history. The Markdown body supplies developer instructions while preserving Codex's built-in instructions.

## Permission policies

Built-in tools use their individual policy, then the agent default (`always_allow`).
MCP tools use the top-level exposed-tool policy, then the server's individual-tool
policy, then the server default. Without one of those policies, MCP tools use
`always_ask`; the agent default does not apply to MCP tools:

```yaml
permission_policy: { type: always_allow }
tool_policies:
  pi.bash: { type: always_ask }
mcp_servers:
  - type: url
    name: notion
    url: https://mcp.notion.com/mcp
    permission_policy: { type: always_ask }
    tool_policies:
      notion-search: { type: always_allow }
```

Top-level tool names use the exposed name, such as `shell`, `pi.bash`,
`claude.Bash`, or `exo_mcp__notion__notion-search`. Entries under an MCP server use
its original tool names. Use `allowed_tools` or `blocked_tools` to disable tools.

For saved managed agents, definition permissions override thread permissions. Each
turn lists artifacts and reads the saved definition, so edits apply to the next
turn on existing local and HTTP threads. Agents without a saved definition keep
using their thread permissions.

The CLI shows the tool and arguments before asking. Enter `y` to allow once,
`n` to deny, or `a` to allow that tool for the current session. Denial becomes a
tool error so the agent can continue; Ctrl+C cancels the turn. Requests and
responses are saved in thread events. Session-wide allowances survive reconnecting
to the same session; changing a policy to `always_ask` does not revoke an existing
allowance. Local and HTTP providers use the same flow, and inline chat reconnects
to a saved HTTP turn's pending approval.

Policies cover basic/RLM tools, registered TypeScript tools, Pi native tools, MCP
tools, and Claude Code's native `PreToolUse` hook. Custom TypeScript harnesses that
execute their own tools must declare `nativeToolApprovals: true`, validate their
active tool inventory with `validateToolPolicies`, and call `context.authorizeTool`
before execution. `context.executeTool` already enforces the policy. Harness
implementations remain trusted code.
Codex and Cursor currently reject native `always_ask` policies that their Exo
adapters cannot enforce. Codex supports policies on its runtime MCP tools; its
native shell does not support approval policies in this adapter. Keep its agent
default `always_allow` and apply MCP tool overrides.

## Environments

An environment is a saved sandbox definition. Select one by file or provider-local
name; each new thread gets its own sandbox:

```sh
exo environment create pi-local --file exoharness/examples/environments/pi-local.yaml
exo environment list
exo environment get pi-local
exo agent run --agent-file exoharness/examples/managed-agents/pi-assistant.md \
  --environment pi-local
```

The example uses Apple container. Change `config.provider` to `docker` on Linux.
`config.image` names the container image (a tag or digest). With SmolVM on macOS,
Exo imports matching images from the local Docker store into its private cache;
registry images are handled by SmolVM. You do not need to export an archive.
Building an image remains a separate step from selecting it in the environment.
For Codex on SmolVM, build `exo-codex-sandbox:latest` with Docker and select
`--environment-file exoharness/examples/environments/codex-smolvm.yaml`.
Definitions forward the existing sandbox settings: `provider`, `image`,
`resources`, `default_workdir`, `file_system_mounts`, `durable_file_systems`, `policy`,
`enable_networking`, and `idle_seconds`. `policy.networking` takes precedence over
`enable_networking`. Unsupported network policies are rejected by the backend.
Omitting `resources` preserves the container backend's defaults; Firecracker uses
its default VM size. Local-process execution has no container resource or filesystem isolation.

Use `--environment-file path.yaml` without saving a definition. An HTTP provider
receives the definition's contents and provisions it on its host. Mount paths in
that spec must be absolute paths on the runtime host. The CLI's `--mount` option
can add local mounts at thread creation; it is rejected for HTTP providers.
The OSS HTTP bearer grants runtime-owner access, including saving environments,
mounting host paths, and local-process execution. Give it only to trusted runtime
operators.

`exo environment update NAME --file path.yaml` changes the saved definition for
new threads. Resume with `--agent NAME --thread THREAD --environment NAME` or
`--environment-file path.yaml` to apply an updated definition to a saved thread.
A changed definition replaces its sandbox and preserves thread history and
filesystem resources; files outside persistent mounts are discarded. Omitting
both environment flags retains the thread's saved configuration. Reapplying the
same definition reuses its sandbox. To upgrade the image of an existing sandbox,
change the image reference in the environment; use a versioned tag or digest.
`exo environment delete NAME` removes only the definition. Explicit host mounts can share data between sandboxes; ordinary sandbox files are private
to their thread. Persistence after a backend terminates a sandbox still follows
that backend's existing lifecycle and durable-file-system support.

## Pi

```sh
container build -t exo-pi-sandbox:latest exoharness/containers/pi-sandbox
exo vault secret create global openai --token-env OPENAI_API_KEY
exo model create gpt-5-mini --secret openai
exo agent run --agent-file exoharness/examples/managed-agents/pi-assistant.md \
  --environment-file exoharness/examples/environments/pi-local.yaml
```

Pi runs inside the sandbox using its RPC mode. The Exo extension forwards native
tool approval requests and declared MCP calls to the runtime. Assistant text
streams live, and each model step contributes token and cost usage. Saved threads
replay Exo history and reuse their environment's sandbox files.

The image pins Pi 0.85.1. The live test exercises local and HTTP managed agents:

```sh
cargo test -p exo --test container_live pi_managed_local_and_http -- --ignored --nocapture
```

It requires Apple container, the Pi image, and `OPENAI_API_KEY`. The host egress
proxy substitutes the model credential; Pi receives only a sandbox placeholder.

## Vaults

For authenticated MCP servers, save a credential once and select its vault when
starting a thread. For example, with `GITHUB_TOKEN` already set:

```bash
exo vault create personal
exo vault secret create personal github \
  --mcp-server-url https://api.githubcopilot.com/mcp/ \
  --token-env GITHUB_TOKEN
unset GITHUB_TOKEN

exo agent run --agent-file exoharness/examples/managed-agents/github-analyst.md \
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
agent. `--vault personal` attaches that vault to a new or resumed thread; repeat
`--vault` to attach more than one. On resume, attachments are additive and
duplicates are ignored. Later attachments take precedence when initially selecting an MCP
credential for the same destination; existing selections stay pinned.

Bindings identify both the vault and secret. A personal MCP credential cannot
shadow a model credential with the same name. Thread attachments and selected MCP
secret references survive resume and fork:

```bash
exo agent run --agent support --vault personal
exo agent run --agent support --thread <thread-slug>
```

To attach a vault without starting a turn:

```bash
exo thread update support <thread-slug> --vault personal
```

Adding a vault preserves the thread's history, environment, resources, and selected
MCP credentials. It does not rebind existing credentials or change a resource's
saved credential reference. Resume with `--agent-file <file> --thread <thread-slug>`
to apply a Git resource's updated `credential` name. Credential-only changes preserve
the existing checkout, branches, commits, and uncommitted work. Git commands use
that credential through the sandbox's egress proxy; real tokens stay outside the
sandbox. Changing MCP destinations requires a new thread.
Revoked secrets fail instead of switching to another account. Agent and thread
records retain vault references, not copies of secret values.

Model bindings use the global vault unless `--vault` selects a different one:

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

GitHub repository resources with a vault `credential` also configure `GH_TOKEN`
for GitHub API access through the egress proxy. `gh` receives a placeholder; the
real token stays in the vault, so no `gh auth login` is needed inside the sandbox.
The default Codex image includes `gh`; other images can install it if needed.
Restricted environment network policies must allow `github.com` and
`api.github.com`. GitHub resources must share a credential for automatic
`GH_TOKEN` selection.

Vault-backed chat requires an isolated sandbox. Local-process execution and mounts
that expose the vault store or its key are rejected. Harness implementations remain
trusted code. Codex, Claude Code, and Pi model keys use the environment's
[credential proxy](../../docs/egress.md#agent-model-credentials) on Apple container,
Docker, or Firecracker, including when the CLI connects to the OSS HTTP runtime.
The wrappers receive placeholders; the runtime keeps the real keys in the vault.
Other sandbox providers still reject credential substitution.

`VaultContext` provides lookup and listing on the harness, agent, and thread.
`ExoHarness` also creates and deletes vaults. `ResourceScope` is shared with
sandboxes; vaults have global, agent, and thread contexts. `VaultHandle` owns
`list_secrets`, `put_secret`, `get_secret`, `update_secret`, and `delete_secret`.
`SecretMetadata` includes an optional destination and a revision. The MCP client
uses `VaultHandle::resolve_secret` to check the destination and read the current
value together.

## Runtime providers

The same CLI commands drive local Exo and an HTTP runtime:

```sh
exo provider create local --local-root .exo
exo provider create oss --url http://localhost:8080/exo
exo provider switch oss
exo agent create analyst --file ./analyst.md
exo agent run --agent analyst --prompt "Summarize this project"
exo thread list analyst
exo agent run --agent analyst --thread <thread-slug>
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

HTTP `--agent-file` runs sync saved agents through the same API as explicit
creation and updates. Saved HTTP turns continue after the CLI disconnects;
reopening their thread recovers durable history, not missed streaming previews.

`GET /agent/{agent_id}/thread/{thread_id}/turn/{turn_id}` under the runtime base URL
returns an `active` boolean, using the same bearer authentication and agent/thread
validation as other runtime endpoints. Reconnection skips saved turns whose provider
is no longer running them.

The workflow tests launch the real CLI and OSS HTTP service with a local model
fixture, so they need no external deployment or model credentials:

```sh
cargo test -p exo --test provider_workflow --test vault_oauth
cargo test -p exo-mcp --test http deepwiki_live -- --ignored
```

The second command separately tests anonymous access to the public DeepWiki MCP.
