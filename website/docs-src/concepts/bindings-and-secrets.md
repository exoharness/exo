---
title: Vaults and credentials
description: Select credentials independently of the agent definition.
---

# Vaults and credentials

Vaults store API keys and OAuth tokens. Agent specs refer to their names or IDs;
the spec contains no secret values.

```bash
exo vault secret create global openai --token-env OPENAI_API_KEY --http-origin https://api.openai.com
```

Select the model and credential in the agent spec:

```yaml
config:
  model: gpt-5.5
  credential: openai
```

Attach another vault with `exo agent run --agent assistant --vault personal`.
Later attachments override same-named entries from earlier vaults, including
`global`. Missing credentials fail instead of falling back to host API keys.
MCP declarations and sandbox provider bindings also use vault credentials.

Sandbox credential policies supply placeholders to the sandbox. Supported
network adapters substitute the real credential only at its authorized
destination. Vault values remain in the runtime; rotation and revocation apply
to subsequent requests.
