---
title: Vaults
description: Credential presets, OAuth, per-user vaults, destination policies, and substitution.
---

# Vaults

A **vault** is a named collection of secrets, such as API keys and OAuth credentials, stored safely at rest. Each secret
has a name and a policy describing where it can be used.

Every thread inherits `global`. On an authenticated server, this is one server-wide shared vault; its owner controls writes.
Threads also inherit the signed-in user's `personal` vault, and you can explicitly attach other vaults to a thread.
Secrets in `global` are shared with all admitted users; keep private integration credentials in a user's vault.
When multiple vaults contain a matching credential, personal credentials take precedence over `global`, and explicitly
attached vaults take precedence over both. Later attachments take precedence over earlier ones.

Secret values are encrypted on disk under the runtime's state directory. On macOS, Exo keeps the encryption master key
in Keychain; it doesn't store each secret value there.

Missing model credentials fail instead of falling back to host API keys.

## Creating and selecting credentials

With `OPENAI_API_KEY` set, save a model credential:

```bash
exo vault secret create global --preset openai
```

This creates a secret named `openai` with permission to access `https://api.openai.com`. Select it in the agent definition:

```yaml
model:
  name: gpt-6.1-sol
  credential: openai
```

`model.credential: openai` selects the secret from the thread's vaults.
The preset is short-hand for:

```bash
exo vault secret create global openai \
  --token-env OPENAI_API_KEY --allow-origin https://api.openai.com
```

To add a GitHub credential:

```bash
exo vault secret create global --preset github
```

The GitHub preset uses `gh` to access your login and permits credential use at `https://github.com` and `https://api.github.com`.
It creates a secret named `github`. With `GITHUB_PAT` set, you can import a token instead:

```bash
exo vault secret create global --preset github --token-env GITHUB_PAT
```

Git resources select this credential explicitly with `credential: github`; it isn't inferred from the repository URL.
Public repositories can be cloned without it. See [Resources](../agent-definition#resources).

To set up the MCP servers in an agent file:

```bash
exo vault secret create --from-agent agent.md
```

Exo skips credentials already available in the target vault, then walks through login for servers that advertise OAuth.
Servers that work anonymously without OAuth metadata are skipped. Credentials are saved under each server's name,
with permission to access its exact MCP URL.
The target vault defaults to `global` locally and `personal` on an authenticated remote provider. You can select a vault
and a particular server explicitly. If the server uses an API key instead of OAuth, import it from an environment variable:

```bash
exo vault secret create personal braintrust --from-agent agent.md --token-env BRAINTRUST_API_KEY
```

This sets up MCP credentials; model keys and Git credentials still use the presets above.

For an MCP server that supports OAuth, you can also log in using its resource URL:

```bash
exo vault secret create global helpdesk --url https://helpdesk.example.com/mcp
```

Exo discovers the server's OAuth settings and stores the resulting credential with permission to access that URL. MCP
credentials are selected by their destination policies, so the secret does not have to share the server's name.

## Per-user vaults

You can use the same agent for different users or customers, with a separate vault for each. For example, with
`ALICE_GITHUB_PAT` and `BOB_GITHUB_PAT` set:

```bash
exo vault create alice
exo vault secret create alice --preset github --token-env ALICE_GITHUB_PAT

exo vault create bob
exo vault secret create bob --preset github --token-env BOB_GITHUB_PAT
```

After [saving an agent](../agent-definition#saved-agents) as `support-analyst`, select the user's vault when creating their thread:

```bash
exo run support-analyst --vault alice
exo run support-analyst --vault bob
```

Both vaults contain a secret named `github`, so the agent definition stays the same. Each thread uses its selected vault's
credential, which takes precedence over a shared credential with the same name. Shared credentials, e.g. a model API key,
can still come from `global`.

Vaults stay attached when you resume a thread. You can repeat `--vault` to attach multiple vaults, or add another vault when
resuming with `--thread THREAD`.

### Shared identities

::: info Planned
This feature is not implemented yet.
:::

Allowed Slack users can run with shared credentials from `global`, without needing
a private vault. Assign a private vault to a shared identity once, using its email:

```bash
exo vault bind alice --email alice@example.com
```

Adapters resolve users through trusted identity sources and use that same vault
alongside `global` when `use_sender_vault: true` is enabled. Optional external
workspace/user-ID pins can restrict resolution more tightly. Vault access is scoped
to the submitting user's turn; the conversation's history and filesystem remain
shared, including data fetched with personal credentials.
See [Slack private credentials](../adapters/slack#private-credentials).

## Destination policies

`--allow-origin` permits a secret's use at an origin (scheme, host, and port). `--allow-url` permits its use at an exact URL.
Both can be repeated. You can update the policy without replacing the secret:

```bash
exo vault secret update global helpdesk --allow-url https://helpdesk.example.com/mcp
```

For sandbox credentials, both the environment's network policy and the secret's destination policy need to allow the request.
Unrestricted networking still respects each secret's policy.

## Credential substitution

Agents get placeholder credentials like `OPENAI_API_KEY=exo_egress_d81f72ff9a6b4ff2bd4e1bae63c207b0`. Exo substitutes the real
credential outside of the sandbox, on requests to permitted destinations. This lets the agent use a credential without being
able to read its value.

Codex and Claude Code run their native MCP clients inside the sandbox. These clients also receive placeholders, with
substitution performed by the egress proxy. Credential-bearing MCP connections must use HTTPS on port 443, and the MCP
host must be allowed by the environment's network policy. This requires a provider supporting substitution, currently
SmolVM or Firecracker. Harnesses using a host-side MCP client make their MCP requests outside the sandbox instead.

Vault values remain in the runtime; rotation and revocation apply to subsequent requests.
