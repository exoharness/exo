---
title: A Sandboxed Conversation
description: Give your agent a shell in an isolated sandbox.
---

# A Sandboxed Conversation

The default REPL conversation is plain chat. To let the agent run shell
commands, create an agent and a conversation explicitly — conversations can
own a sandbox.

## Create an agent and conversation

```bash
cat > sandbox-example.md <<'EOF'
---
name: "Sandbox Example"
harness: basic
config:
  model: gpt-5.5
  credential: openai
---
Help the user with their task.
EOF
exo agent create sandbox-example --file sandbox-example.md
exo thread create sandbox-example "Local Dev"
cat > sandbox-environment.yaml <<'EOF'
name: dev
config:
  provider: smolvm
  image: ubuntu:24.04
  enable_networking: true
EOF
exo agent run --agent sandbox-example --thread local-dev --environment-file sandbox-environment.yaml
```

The agent can now execute commands in the conversation's sandbox via the
shell tool.

## Choosing a sandbox backend

Set `config.provider` in the environment file to choose a backend:

| Backend | Isolation | Notes |
|:--------|:----------|:------|
| `smolvm` | MicroVM | Default; KVM on Linux or Apple Silicon on macOS |
| `docker` | Container | Requires Docker |
| `apple_container` | Container | macOS |
| `local_process` | **None** | Runs directly on the host |

::: warning
  `local-process` gives the model unrestricted shell access to your machine.
  Use it only when you trust the agent and the task.
:::

Remote sandbox providers (Daytona, E2B, Vercel, Sprites, AWS AgentCore) are
configured as *provider bindings*:

```bash
exo vault secret create global daytona --token-env DAYTONA_API_KEY
exo environment provider create --backend daytona --secret daytona
```

## Sandbox scope and image

Environments give each thread its own sandbox. Set `config.image` in the
file to choose the container image. The thread saves its environment, so later
runs can resume without passing the file again.

You can also run one-off commands in a conversation's sandbox from the CLI:

```bash
exo thread sandbox run sandbox-example local-dev "ls /"
```

Sandboxes can be snapshotted and rewound together with conversation history
— see [Time Travel](../concepts/time-travel).
