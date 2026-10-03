# Managed agents

Write an agent in Markdown, run it locally, and talk to it on the CLI.
Agents use Exo's local state and sandbox providers.

```markdown
---
name: support-analyst
harness: codex
config:
  model: gpt-5.6-sol
  credential: openai
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
| `vault secret`                               | `create`, `update`, `delete`                |
| `agent mount`, `thread mount`                | `create`, `list`, `delete`                  |
| `environment provider`                       | `create`, `list`                            |

Use `exo thread list AGENT` to list saved chats and `exo agent run --agent AGENT --thread THREAD`
to resume one. Environments are reusable configurations; agents and threads manage
the running instances. Runtime-specific scope belongs in the provider URL. Exo preserves its path
and query parameters without interpreting them.

## Setup

Exo home defaults to `$HOME/.exo`, shared across working directories. `--root`
or `EXO_HOME` selects a different home; the flag takes precedence. Provider profiles
and authentication default to `<root>/config`, runtime state to `<root>/exoharness`,
and the pricing cache to `<root>/cache`. Docker and Apple container durable filesystems
live in `<root>/exoharness/durable-filesystems`; `EXO_DURABLE_FILE_SYSTEM_ROOT`
overrides that location. Local TypeScript harnesses receive the
resolved home as `EXO_HOME`. `--config-dir` overrides the profile directory;
`--master-key-path` overrides the file encryption key, which otherwise lives at
`<root>/exoharness/master.key`. Explicit provider selections and saved
remote aliases continue to select their configured state or server.
State created under the previous `./.exo` default stays in that directory;
use `--root /absolute/path/to/previous-checkout/.exo` to access it. Exo does not
move it automatically.

For provider profiles saved under the previous `~/.config/exo` default, pass
`--config-dir ~/.config/exo`. For existing file-encrypted state, keep using its
original key with `--master-key-path /path/to/master.key`. macOS Keychain accounts
remain tied to the runtime state directory.

From this checkout:

```bash
pnpm install --frozen-lockfile
cargo build -p exo
export PATH="$PWD/target/debug:$PATH"

exo vault secret create global openai --token-env OPENAI_API_KEY --allow-origin https://api.openai.com
```

`OPENAI_API_KEY` must already be set. `--token-env` takes the variable's name.
Set `config.model` to the upstream model name and `config.credential` to a secret
name or ID in a selected vault. Set `config.base_url` for a custom endpoint.
There is no model registration or fallback to another model. `--model` overrides
the model name for the thread and keeps its credential and endpoint.

Codex, Claude Code, and Pi use digest-pinned devbox images by default. To develop
the Codex image locally, build it and set
`config.image: exo-codex-devbox:latest` in an environment definition:

```bash
docker build --build-arg DEVBOX_TOOLS=true -t exo-codex-devbox:latest \
  exoharness/containers/codex-sandbox
```

For a smaller image without Python and TypeScript tools, use
`ghcr.io/exoharness/codex-sandbox:latest` as the sandbox image.

Exo defaults to SmolVM locally. The CLI's default `smolvm`
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

### Browser previews

Declaring TCP ports in an environment automatically provides browser previews:

```yaml
name: dev
config:
  image: my-dev-image
  tcp_ports: [5173, 8000]
```

`exo agent run` prints a clickable sandbox services page and one URL per port:

```text
sandbox: http://my-project-<id>.localhost:<port>
  port 5173: http://5173.my-project-<id>.localhost:<port>
  port 8000: http://8000.my-project-<id>.localhost:<port>
```

Environments with no published ports have no browser previews. The HTML page
lists every service link. Exo also gives these URLs to the agent,
so it can start services and configure browser API URLs and CORS origins.
`.localhost` names resolve to loopback in browsers without DNS or hosts-file
changes. HTTP and WebSocket paths and application headers pass through unchanged.

An inline run owns one browser listener for its thread. All of that thread's
services share its port; other open threads have their own ports. Resume reuses
the saved port and URLs. Keep the CLI session open while using previews.

With an HTTP provider, `exo serve` owns one shared preview listener for its
threads. Closing a client does not close previews or stop the server's services.
The server saves its listener port across restarts. `exo serve --preview-domain
DOMAIN` sets the advertised DNS suffix for its previews; the default is
`localhost`. Preview listeners bind to `127.0.0.1`. For a remote server, forward
the printed preview port with SSH, using the same port locally:

```sh
ssh -L PORT:127.0.0.1:PORT SERVER
```

Display a thread's URLs without starting a VM:

```sh
exo thread ports AGENT THREAD
```

This command uses the selected provider's preview address. Links require the
owning CLI session or server and the sandbox services to be running. Previews
currently use HTTP. Browser links work for HTTP and WebSocket services; use
`exo thread sandbox forward` for other TCP services.

#### Preview troubleshooting

| Symptom                                      | What to check                                                                                                                                                                                                                  |
| -------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| Connection refused                           | Keep the inline agent session or provider server running. For remote providers, check the SSH forward.                                                                                                                         |
| `404 Unknown preview hostname`               | Use the exact URL printed when opening the thread through this provider.                                                                                                                                                       |
| `502 Bad Gateway`                            | Start the sandbox and its services. Check the guest port and listen on an interface reachable by sandbox forwarding; the FastAPI example uses `0.0.0.0`.                                                                       |
| UI loads, but API or WebSocket requests fail | Use the API service's browser origin. Allow the full frontend origin, including its port, in backend CORS and allow preview hostnames in development-server host checks. Guest-local URLs still work for server-side requests. |
| Saved port is occupied                       | Release the conflicting listener. Exo reports the bind error and preserves the port so browser origins stay stable.                                                                                                            |

Listener startup and accept errors appear in Exo's output; routine browser
disconnects stay at debug level. Service logs stay inside the sandbox; the
FastAPI example writes frontend/API logs under `/var/lib/fastapi-demo/logs` and
PostgreSQL logs to `/var/lib/fastapi-demo/postgres/server.log`. Ask the agent to
inspect them when a service is unavailable. `--verbosity full` includes egress
diagnostics.

### Named agents and threads

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
exo agent run --agent support --thread my-project
```

The CLI prints the agent and thread ids. Both ids and slugs work when resuming.
Without `--thread`, each run starts a new thread. `--thread NAME` creates a thread
with that name on the first run and resumes it on subsequent runs.

New names must contain 1–128 ASCII letters, digits, hyphens, or underscores and
start with a letter or digit; spaces and slashes are rejected. An unknown
UUID-shaped reference returns `thread ... not found` instead of creating a
thread with that name. Startup announces whether it is creating or opening a
thread.

Select VM settings explicitly with `--environment NAME`:

```bash
exo agent create support --file agent.md
exo environment create local-dev --file environment.yaml
exo agent run --agent support --environment local-dev --thread my-project
```

The environment file's `name` must match the name passed to `environment create`.
The selected configuration is saved on the thread; resuming without `--environment`
retains it. Passing `--environment` again applies the current saved definition.
Add `--prompt "..."` to run a single turn and exit.

Direct local CLI sessions stop their managed thread sandboxes when the session
exits. With SmolVM, resuming the thread restarts the same VM with its disks and
chat retained. Processes inside the VM restart; a development stack needs its
startup command on resume. Deleting the thread removes its managed VM and disks.
HTTP clients leave sandbox lifetime with the server, so use `exo serve` when
services should stay running between client sessions.

Local one-off commands, interactive sessions, and provider servers use the same
exclusive thread ownership. Different threads can run under the same state root;
see [Local sandbox lifetime](../../docs/resources.md#local-sandbox-lifetime) for
the command ownership rules and crash recovery behavior.

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
Anthropic vault secret; its default image is the Claude Code devbox. Custom TypeScript
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

The agent spec selects its model and vault credential. Attach a vault with
`--vault team`; secrets in later attachments take precedence over same-named
secrets in earlier attachments, including `global`. Sandbox provider bindings
accept `--vault team --secret KEY` under `exo environment provider create --backend BACKEND`.

Pricing, harness, and egress overrides belong on `agent run`, `thread send`, or
`serve`; configuration and vault commands do not accept them. Use `--env-file FILE`
to load environment variables explicitly; a missing file is an error.

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
`GET /exo/agent/{agent_id}/thread/{thread_id}/previews` returns the server's
`{domain, port}` preview address, or `null` when previews are unavailable.
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
The Codex example at `exoharness/examples/environments/codex-smolvm.yaml` uses
the published `ghcr.io/exoharness/codex-devbox:latest`. To use a locally built
image instead, change its `config.image` to `exo-codex-devbox:latest`.
Definitions forward the existing sandbox settings: `provider`, `image`,
`resources`, `default_workdir`, `file_system_mounts`, `durable_file_systems`, `tcp_ports`, `policy`,
`enable_networking`, and `idle_seconds`. `policy.networking` takes precedence over
`enable_networking`. Omitted `provider` selects SmolVM, and omitted networking
allows unrestricted access. Unsupported network policies are rejected by the backend.
Omitting `resources` preserves the container backend's defaults; Firecracker uses
its default VM size. Local-process execution has no container resource or filesystem isolation.

SmolVM also accepts `resources.storage_gib` and `resources.overlay_gib` to size
its storage and persistent root filesystem disks. Both must be positive integers;
other backends reject these settings. These are virtual disk sizes, not RAM.

Use `--environment-file path.yaml` without saving a definition. An HTTP provider
receives the definition's contents and provisions it on its host. Mount paths in
that spec must refer to directories on the runtime host. Relative `host_path`
values in environment files resolve against the file's directory and are saved
as absolute paths. For HTTP providers, use absolute paths on the server.
The CLI's `--mount` option
can add local mounts at thread creation; it is rejected for HTTP providers.
The OSS HTTP bearer grants runtime-owner access, including saving environments,
mounting host paths, and local-process execution. Give it only to trusted runtime
operators.

`exo environment update NAME --file path.yaml` changes the saved definition for
new threads. Resume with `--agent NAME --thread THREAD --environment NAME` or
`--environment-file path.yaml` to apply an updated definition to a saved thread.
A changed sandbox configuration replaces its sandbox and preserves thread history
and filesystem resources; files outside persistent mounts are discarded. Omitting
both environment flags retains the thread's saved configuration. Reapplying the
same definition reuses its sandbox. To upgrade the image of an existing sandbox,
change the image reference in the environment; use a versioned tag or digest.
`exo environment delete NAME` removes only the definition. Explicit host mounts can share data between sandboxes; ordinary sandbox files are private
to their thread. Persistence after a backend terminates a sandbox still follows
that backend's existing lifecycle and durable-file-system support.

## Development service ports

Declare the guest ports a thread needs in its environment:

```yaml
name: web-dev
config:
  provider: smolvm
  image: my-devbox:latest
  tcp_ports: [3000, 8000]
```

After starting the services in the thread, forward a declared port to the local
machine:

```sh
exo thread sandbox forward my-agent THREAD --port 3000 --bind 127.0.0.1:13000
```

Open `http://127.0.0.1:13000`. The forward carries TCP, including HTTP and
WebSockets, until Ctrl-C. Omitting `--bind` allocates a loopback port and prints
its address. Each thread can use the same guest ports with different local
listeners. Browser requests to additional services need their own forwards and
matching browser URLs, or an application proxy serving those services together.

This command currently requires a local Exo provider and a sandbox backend with
TCP support. SmolVM port access inspects the running VM without restarting it or
taking over its credential proxy. Changing an environment's `tcp_ports` or
`resources` replaces its sandbox when the updated environment is applied to the
thread.

### FastAPI development example

This example uses the shared Codex devbox and a public repository with a frontend
on port 5173, an API on port 8000, and PostgreSQL inside the VM. With an OpenAI
credential saved as `openai` in the global vault, run from this checkout:

```sh
exo agent create fastapi-dev --file exoharness/examples/managed-agents/fastapi-developer.md
exo environment create fastapi-local --file exoharness/examples/environments/fastapi-smolvm.yaml
exo agent run --agent fastapi-dev --environment fastapi-local --thread demo
```

Ask the agent to start the services, then open the printed sandbox services page.
The browser contacts the API preview directly; the setup script configures its
URL and the backend's allowed frontend origin. Log in with `admin@example.com`
and `changethis`. Initial setup installs the tools and locked dependencies on the
VM's persistent disks. Resume with `exo agent run --agent fastapi-dev --thread demo`
and ask the agent to start the services again.

## Pi

```sh
exo vault secret create global openai --token-env OPENAI_API_KEY --allow-origin https://api.openai.com
exo agent run --agent-file exoharness/examples/managed-agents/pi-assistant.md \
  --environment-file exoharness/examples/environments/pi-local.yaml
```

The environment file uses the published `ghcr.io/exoharness/pi-devbox:latest`.
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

List vaults with `exo vault list`, their contents with `exo vault list personal`,
and a secret's metadata with `exo vault get personal github`. These commands
never print credential values.

```sh
exo vault create personal
exo vault secret create personal --preset github
exo vault secret create personal --preset openai
exo vault secret create personal notion --url https://mcp.notion.com/mcp
```

A preset supplies a credential policy and a default secret name. An explicit
secret name overrides the preset's default; without a preset, the name is
required. Import an existing token with `--token-env`. The OpenAI preset reads
`OPENAI_API_KEY` and permits `https://api.openai.com`; combine it with
`--token-env` to read a different variable. To rotate or reauthorize an existing
credential, use `secret update` with the same options; its identity and policy
are preserved unless explicitly changed.

The GitHub preset uses GitHub CLI (`gh`) 2.81 or newer. For a linked local vault,
`gh` runs on the runtime host. Exo checks the linked account's token on first use
after restart, then caches it for 24 hours, rechecking on demand. Authentication
rejection in the proxy or MCP client forces an immediate check. Switching the
active account in `gh` does not change the linked account. Reading `gh` does not
renew an expired token: authenticate the same account with `gh auth login` when
GitHub requires a new login.

Sharing a vault containing a linked account allows authorized callers to use
that account's credentials within their permitted destinations. Authenticated
callers have private default vaults; the operator's global vault is not
automatically shared.

Remote vaults receive a token copy because they cannot access your local `gh`;
use `exo vault secret update personal github --preset github` to copy a replacement.
`--token-env` also stores a token copy. Supply `--client-id` to use your own
GitHub application's device flow and store its OAuth grant instead. The
application must enable device flow and have the repository permissions you need.
`--scope` requests OAuth scopes; with `gh`, it adds permissions to its existing
token rather than narrowing them.

OAuth settings are shared options, not custom preset definitions: `--client-id`,
`--client-secret-env`, and repeated `--scope` work with resource discovery.
For other device-flow servers, supply `--device-url`, `--token-url`, and
`--client-id`, plus a credential policy. OAuth grants retain expiry and refresh
credentials in encrypted storage.

`--allow-origin` permits an entire HTTPS origin, including its port (loopback
HTTP is also supported for local development);
`--allow-url` permits an exact resource URL, including its path and query. Both
are repeatable and apply to token imports, policy updates, and login. `--policy`
accepts a JSON, YAML, or TOML credential policy instead. The environment's network
policy remains a separate restriction and cannot widen credential permissions.

```sh
exo vault secret create personal openai --token-env OPENAI_API_KEY \
  --allow-origin https://api.openai.com
exo vault secret update personal github \
  --allow-origin https://github.com --allow-origin https://api.github.com
```

Policy-only updates preserve the credential value. Login never attaches a vault
to an agent; continue selecting vaults with the agent specification or `--vault`.

For authenticated MCP servers, save a credential once and select its vault when
starting a thread. For example, with `GITHUB_TOKEN` already set:

```bash
exo vault create personal
exo vault secret create personal github \
  --allow-url https://api.githubcopilot.com/mcp/ \
  --token-env GITHUB_TOKEN
unset GITHUB_TOKEN

exo agent run --agent-file exoharness/examples/managed-agents/github-analyst.md \
  --vault personal
```

`--token-env` reads a variable from the process environment or `--env-file` once.
It accepts a variable name, not a token. Subsequent chats don't need that variable.
Exact URL policies match the agent's MCP URL including its path, trailing slash,
and query; origin policies allow any resource on that origin. Names are labels.
Within each vault, an exact URL match takes precedence over an origin match;
equally specific matches are rejected as ambiguous. Later attached vaults still
override earlier vaults. Servers without a matching credential connect unauthenticated.

```bash
exo vault list
exo vault get personal
exo vault list personal
exo vault get personal github
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

Store a model credential in the global vault, then name it in the agent's
`config.credential`:

```bash
exo vault secret create global openai --token-env OPENAI_API_KEY --allow-origin https://api.openai.com
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
The default Codex, Claude Code, and Pi images include `gh`.
Restricted environment network policies must allow `github.com` and
`api.github.com`. GitHub resources must share a credential for automatic
`GH_TOKEN` selection.

Vault-backed chat requires an isolated sandbox. Local-process execution and mounts
that expose the vault store or its key are rejected. Harness implementations remain
trusted code. Codex, Claude Code, and Pi model keys use the environment's
[credential proxy](../../docs/egress.md#agent-model-credentials) on SmolVM or
Firecracker, including when the CLI connects to the OSS HTTP runtime.
The wrappers receive placeholders; the runtime keeps the real keys in the vault.
SmolVM does not support credential substitution when restoring a snapshot.
Other sandbox providers reject credential substitution.

`VaultContext` provides lookup and listing on the harness, agent, and thread.
`ExoHarness` also creates and deletes vaults. `ResourceScope` is shared with
sandboxes; vaults have global, agent, and thread contexts. `VaultHandle` owns
`list_secrets`, `put_secret`, `get_secret`, `update_secret`, and `delete_secret`.
`SecretMetadata` includes an optional destination and a revision. The MCP client
uses `VaultHandle::resolve_secret` to check the destination and read the current
value together.

### Upgrading credential policies

- Existing vault catalogs are rewritten once on first open; even a metadata-only command needs master-key access for this migration.
- Saved sandboxes with the previous credential bindings are recreated on first resume.
- `--http-origin` and `--mcp-server-url` are no longer accepted: use `--allow-origin`/`--allow-url` for token policies, or `--url` for OAuth discovery.

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
provider selects the harness, connects MCP servers, and resolves model credentials from selected vaults
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
