# Coding Agent Harnesses

The Codex, Claude Code, and Cursor examples treat exoharness events as canonical
conversation state and run their native agent runtimes inside configured
exoharness sandboxes.

Install dependencies and build the CLI first:

```bash
pnpm install
cargo build -p exo
```

The examples below use `./target/debug/exo`. If you have the binary on your
`PATH`, you can use `exo` instead.

The `codex`, `claude-code`, and `cursor` harness presets select the matching
TypeScript module, sandbox image, and networking defaults.

For `secret set`, `--env` takes the variable name literally. For example, use
`--env OPENAI_API_KEY`, not `--env $OPENAI_API_KEY`.

The sandbox image commands use Apple container. It currently requires an Apple
silicon Mac running macOS 26 or newer.

Install Apple container:

1. Download the latest signed installer package from
   <https://github.com/apple/container/releases>.
2. Open the package and follow the installer prompts. It installs files under
   `/usr/local` and may ask for an administrator password.
3. Start the container system service:

```bash
container system start
```

For upgrades, downgrades, uninstall instructions, and building from source, see
<https://github.com/apple/container>.

## Codex

Register an OpenAI model:

```bash
./target/debug/exo secret set openai --env OPENAI_API_KEY
./target/debug/exo model register gpt-5.5 --secret openai
```

Build the sandbox image:

```bash
container build \
  --platform linux/arm64 \
  -t exo-codex-sandbox:latest \
  exoharness/containers/codex-sandbox
```

Create the agent and start a conversation:

```bash
./target/debug/exo --harness codex agent create "TS Codex" \
  --model gpt-5.5

./target/debug/exo conversation create ts-codex
./target/debug/exo conversation mount add ts-codex <conversation> "$PWD" /workspace --rw
./target/debug/exo repl --agent ts-codex --conversation <conversation>
```

### Codex on an Anthropic model

Codex only speaks the OpenAI Responses API, so an Anthropic model reaches it
through a gateway that serves that API, such as a LiteLLM proxy
(`litellm --model claude-sonnet-4-6`, reachable from Docker sandboxes at the
bridge gateway, usually `http://172.17.0.1:4000/v1`). The Harbor eval starts one
itself, and `python -m exo_harbor.gateway <model>` from the eval's virtualenv
runs the same gateway for a plain agent; see `eval/harbor/README.md`. Register
the model with the gateway's base URL. Codex ignores `OPENAI_BASE_URL`, so the
harness turns a binding's base URL into a Codex model provider (`-c` overrides
on app-server) and starts threads on it; OpenRouter gets Chat Completions since
it has no Responses API.

```bash
./target/debug/exo secret set anthropic --env ANTHROPIC_API_KEY
./target/debug/exo model register claude-sonnet-4-6 --secret anthropic \
  --base-url http://172.17.0.1:4000/v1

./target/debug/exo --harness codex agent create "Codex on Claude" \
  --model claude-sonnet-4-6
```

## Claude Code

Register an Anthropic model:

```bash
./target/debug/exo secret set anthropic --env ANTHROPIC_API_KEY
./target/debug/exo model register claude-sonnet-4-6 --secret anthropic
```

Build the sandbox image:

```bash
container build \
  --platform linux/arm64 \
  -t exo-claude-code-sandbox:latest \
  exoharness/containers/claude-code-sandbox
```

Create the agent and start a conversation:

```bash
./target/debug/exo --harness claude-code agent create "TS Claude Code" \
  --model claude-sonnet-4-6

./target/debug/exo conversation create ts-claude-code
./target/debug/exo conversation mount add ts-claude-code <conversation> "$PWD" /workspace --rw
./target/debug/exo repl --agent ts-claude-code --conversation <conversation>
```

The harness runs Claude Code headless with permission prompts bypassed; the
exoharness sandbox is the boundary. In Claude Code's default mode a run with
nobody to approve prompts has every file edit and most shell commands denied.

### Claude Code on a non-Anthropic model

Claude Code only speaks the Anthropic Messages API, so any other model reaches
it through a gateway that serves the model in that format: OpenRouter does for
its whole catalog, and a LiteLLM proxy (`litellm --model gpt-5.5`, reachable
from Docker sandboxes at the bridge gateway, usually `http://172.17.0.1:4000`)
or Ollama work the same way. The Harbor eval starts a LiteLLM gateway itself
when the model is not Anthropic's; see `eval/harbor/README.md`. Register the
model with the gateway's key and base URL; a base URL that is not
`api.anthropic.com` switches the harness into gateway mode:

```bash
./target/debug/exo secret set openrouter --env OPENROUTER_API_KEY
./target/debug/exo model register openai/gpt-5.5 --secret openrouter \
  --base-url https://openrouter.ai/api/v1

./target/debug/exo --harness claude-code agent create "Claude Code on GPT" \
  --model openai/gpt-5.5
```

The OpenAI-style `/v1` base URL is the same one exo's other harnesses take for
OpenRouter, so one binding serves them all; the harness drops that segment
because Claude Code appends `/v1/messages` itself. In gateway mode the harness
also passes the key as a bearer token, pins Claude Code's Haiku, Sonnet, Opus,
and subagent model slots to the registered model so background calls do not
ask the gateway for a Claude model it cannot serve, and turns off Claude Code's
nonessential traffic.

## Cursor

Register a Cursor model:

```bash
./target/debug/exo secret set cursor --env CURSOR_API_KEY
./target/debug/exo model register auto --secret cursor
```

Build the sandbox image:

```bash
container build \
  --platform linux/arm64 \
  -f exoharness/containers/cursor-sdk-sandbox/Containerfile \
  -t exo-cursor-sdk-sandbox:latest \
  .
```

Create the agent and start a conversation:

```bash
./target/debug/exo --harness cursor agent create "TS Cursor" \
  --model auto

./target/debug/exo conversation create ts-cursor
./target/debug/exo conversation mount add ts-cursor <conversation> "$PWD" /workspace --rw
./target/debug/exo repl --agent ts-cursor --conversation <conversation>
```

## Pi

Register a model Pi supports. Pi reads the provider key from the sandbox
environment, so the same variable has to be set where exo runs:

```bash
./target/debug/exo secret set openai --env OPENAI_API_KEY
./target/debug/exo model register gpt-5.5 --secret openai
```

Build the sandbox image:

```bash
container build \
  --platform linux/arm64 \
  -t exo-pi-sandbox:latest \
  exoharness/containers/pi-sandbox
```

Create the agent and start a conversation:

```bash
./target/debug/exo --harness pi agent create "TS Pi" \
  --model gpt-5.5

./target/debug/exo conversation create ts-pi
./target/debug/exo conversation mount add ts-pi <conversation> "$PWD" /workspace --rw
./target/debug/exo repl --agent ts-pi --conversation <conversation>
```

An agent model of `provider/model` selects a provider explicitly; a bare name
is read as OpenAI. Pi otherwise picks its own default, which is an Anthropic
model, and a run configured for another provider then produces nothing.

## Live E2E

The live e2e script runs replay checks against the coding-agent harnesses:

```bash
pnpm e2e:agent-harnesses --only codex
pnpm e2e:agent-harnesses --only claude
pnpm e2e:agent-harnesses --only cursor
pnpm e2e:agent-harnesses --only pi
```

Use `--build-images` to build the required sandbox images before running.
