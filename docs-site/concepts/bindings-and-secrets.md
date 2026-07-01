---
title: Bindings & Secrets
parent: Concepts
nav_order: 5
---

# Bindings & Secrets

Exo splits credentials into two records so that configuration can be
shared, inspected, and versioned without ever exposing key material.

## Secrets

Secrets hold **only credential material** — opaque API keys, OAuth tokens.
They live in the exoharness secret store (file-backed by default, Apple
Keychain supported):

```bash
exo secret set openai --env OPENAI_API_KEY   # reads the variable name literally
exo secret set openai --value "$OPENAI_API_KEY"  # stores the expanded value
```

## Bindings

Bindings are **non-secret configuration that refer to secrets**:

- an *env var binding* maps a variable name to a secret,
- an *LLM binding* defines a provider/model plus optional credentials
  (`exo model register`),
- a *sandbox binding* defines a sandbox provider plus its credentials
  (`exo provider configure`),
- an *MCP binding* defines a server URL plus optional credentials.

## Scoping

Executors, agents, and individual conversations can all define bindings and
secrets. **Conversation-scoped values override agent-scoped values**, so a
single agent can talk to different endpoints or use different credentials
per conversation.

## Keeping secrets away from the model

Secrets can be used without the LLM's knowledge — for example to
authenticate MCP servers — or securely mounted inside sandboxes so
specific programs can access them while the LLM can neither view nor
exfiltrate them. This is a direct payoff of the
[exoharness/executor split]({% link concepts/exoharness-and-executor.md %}):
the layer the agent can modify never holds the keys.
