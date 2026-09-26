# Managed-agent provider examples

## Connect an agent to Notion through a vault

Run from the Exo repo using the local provider:

```sh
exo vault create personal
exo vault secret create personal notion \
  --mcp-server-url https://mcp.notion.com/mcp
exo agent run --agent-file exoharness/examples/managed-agents/notion-analyst.md \
  --vault personal
```

The create command opens Notion's OAuth authorization page. Approve the workspace
you want the agent to access. The grant is encrypted in the vault, and Exo
refreshes expiring tokens when it uses the MCP server. You can restart the CLI
and run the same chat command without logging in again. Use `--no-browser` to
open the printed authorization URL yourself.

Ask it to find a page and summarize it with links to its sources. MCP tools
require approval by default: the CLI shows the tool and arguments, then asks
whether to allow once, deny, or allow that tool for the session. This is separate
from the model asking for confirmation in conversation. Built-in tools default
to `always_allow`.

To allow all calls to a trusted MCP server without prompting, add
`permission_policy: {type: always_allow}` to that server's `mcp_servers` entry.
Use `allowed_tools` or `blocked_tools` to restrict which tools are available.

To reconnect an expired or revoked grant, or remove the local credential:

```sh
exo vault secret update personal notion
exo vault secret delete personal notion
```

Removal prevents subsequent calls through that vault entry; it does not revoke
the grant at Notion. To supply a static bearer token instead of using OAuth,
add `--token-env VARIABLE_NAME` to `secret create` or `secret update`.

Vault credentials match MCP servers by URL, including the path, trailing slash, and query. The secret's name and
the agent's MCP name can differ.

## Log in to an OAuth runtime

For an Exo-compatible runtime that advertises OAuth metadata, set
`EXO_RUNTIME_URL` to its runtime API URL, then run:

```sh
exo provider create remote --url "$EXO_RUNTIME_URL"
exo provider login remote
exo --provider remote agent list
exo provider logout remote
```

Login opens a browser and uses a localhost callback with PKCE. Use
`exo provider login remote --no-browser` to open the printed URL yourself.
If the authorization server requires a registered client, supply its client ID
with `provider create --client-id`. Scopes are discovered from the server unless
you supply `--scope` when configuring the provider.

Credentials are saved in the OS credential store after the runtime confirms
your account. Expiring tokens refresh before HTTP requests, including during an
existing chat. Logout removes the saved credentials; existing clients must log
in again. Headless clients can use `--api-key-env` when configuring a provider that requires
bearer authentication.

Provider login authenticates the CLI to a runtime. Use vault credentials for
external MCP servers, as in the Notion example above.

## Work on a repository

Save an agent with a Git resource so every new thread starts with its own checkout:

```sh
exo agent create exo-dev --file exoharness/examples/managed-agents/exo-developer.md
exo agent run --agent exo-dev
```

The shared Git checkout is refreshed before each new thread and its volume is
cloned with copy-on-write. See [filesystem resources](../../../docs/resources.md)
for local directories, vault credentials and lifecycle details. Choose an
environment containing the repository's build tools when compiling code; the
standard Codex image does not include Rust.
