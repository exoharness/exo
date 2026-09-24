# Managed-agent examples

## Connect an agent to Notion through a vault

Run from the Exo repo using the local provider:

```sh
exo vault create personal
exo vault secret create personal notion \
  --mcp-server-url https://mcp.notion.com/mcp
exo chat --agent-file exoharness/examples/managed-agents/notion-analyst.md \
  --vault personal
```

The create command opens Notion's OAuth authorization page. Approve the workspace
you want the agent to access. The grant is encrypted in the vault, and Exo
refreshes expiring tokens when it uses the MCP server. You can restart the CLI
and run the same chat command without logging in again. Use `--no-browser` to
open the printed authorization URL yourself.

Ask it to find a page and summarize it with links to its sources. The example
asks before writes; it does not enforce a read-only tool policy.

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
