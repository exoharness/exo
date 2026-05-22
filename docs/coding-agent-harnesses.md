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
./target/debug/exo model register gpt-5.4 --secret openai
```

Build the sandbox image:

```bash
container build \
  --platform linux/arm64 \
  -t exo-codex-sandbox:latest \
  containers/codex-sandbox
```

Create the agent and start a conversation:

```bash
./target/debug/exo --harness typescript agent create "TS Codex" \
  --module examples/typescript/codex-harness.ts \
  --model gpt-5.4 \
  --sandbox-image exo-codex-sandbox:latest \
  --networking enabled

./target/debug/exo conversation create ts-codex
./target/debug/exo conversation mount add ts-codex <conversation> "$PWD" /workspace --rw
./target/debug/exo chat repl ts-codex <conversation>
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
  containers/claude-code-sandbox
```

Create the agent and start a conversation:

```bash
./target/debug/exo --harness typescript agent create "TS Claude Code" \
  --module examples/typescript/claude-code-harness.ts \
  --model claude-sonnet-4-6 \
  --sandbox-image exo-claude-code-sandbox:latest \
  --networking enabled

./target/debug/exo conversation create ts-claude-code
./target/debug/exo conversation mount add ts-claude-code <conversation> "$PWD" /workspace --rw
./target/debug/exo chat repl ts-claude-code <conversation>
```

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
  -f containers/cursor-sdk-sandbox/Containerfile \
  -t exo-cursor-sdk-sandbox:latest \
  .
```

Create the agent and start a conversation:

```bash
./target/debug/exo --harness typescript agent create "TS Cursor" \
  --module examples/typescript/cursor-sdk-harness.ts \
  --model auto \
  --sandbox-image exo-cursor-sdk-sandbox:latest \
  --networking enabled

./target/debug/exo conversation create ts-cursor
./target/debug/exo conversation mount add ts-cursor <conversation> "$PWD" /workspace --rw
./target/debug/exo chat repl ts-cursor <conversation>
```

## Live E2E

The live e2e script runs replay checks against the coding-agent harnesses:

```bash
pnpm e2e:agent-harnesses --only codex
pnpm e2e:agent-harnesses --only claude
pnpm e2e:agent-harnesses --only cursor
```

Use `--build-images` to build the required sandbox images before running.
