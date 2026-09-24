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

For `secret create`, `--env` takes the variable name literally. For example, use
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

The built-in TypeScript harness constructors resolve bundled runners and modules
from the source checkout used to build Exo, for both Basic and Exo tool runtimes.
Keep that checkout installed; embedders can pass an explicit workspace root to
`TypeScriptHarness::new`.

## Codex

Usage counts upstream `rawResponse/completed` records, including local replay
compaction. Codex's remote compaction endpoint does not expose billable usage;
its context-size estimates are excluded from token and cost totals.

Register an OpenAI model:

```bash
./target/debug/exo secret create openai --env OPENAI_API_KEY
./target/debug/exo model create gpt-5.5 --secret openai
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
./target/debug/exo agent --harness codex create "TS Codex" \
  --model gpt-5.5

./target/debug/exo thread create ts-codex
./target/debug/exo thread mount create ts-codex <conversation> "$PWD" /workspace --rw
./target/debug/exo chat --agent ts-codex --thread <conversation>
```

## Claude Code

Register an Anthropic model:

```bash
./target/debug/exo secret create anthropic --env ANTHROPIC_API_KEY
./target/debug/exo model create claude-sonnet-4-6 --secret anthropic
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
./target/debug/exo agent --harness claude-code create "TS Claude Code" \
  --model claude-sonnet-4-6

./target/debug/exo thread create ts-claude-code
./target/debug/exo thread mount create ts-claude-code <conversation> "$PWD" /workspace --rw
./target/debug/exo chat --agent ts-claude-code --thread <conversation>
```

## Cursor

Register a Cursor model:

```bash
./target/debug/exo secret create cursor --env CURSOR_API_KEY
./target/debug/exo model create auto --secret cursor
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
./target/debug/exo agent --harness cursor create "TS Cursor" \
  --model auto

./target/debug/exo thread create ts-cursor
./target/debug/exo thread mount create ts-cursor <conversation> "$PWD" /workspace --rw
./target/debug/exo chat --agent ts-cursor --thread <conversation>
```

## Pi

Pi sandbox images must define `HOME` as a writable directory for session and tool files.

Register a model Pi supports. Pi reads the provider key from the sandbox
environment, so the same variable has to be set where exo runs:

```bash
./target/debug/exo secret create openai --env OPENAI_API_KEY
./target/debug/exo model create gpt-5.5 --secret openai
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
./target/debug/exo agent --harness pi create "TS Pi" \
  --model gpt-5.5

./target/debug/exo thread create ts-pi
./target/debug/exo thread mount create ts-pi <conversation> "$PWD" /workspace --rw
./target/debug/exo chat --agent ts-pi --thread <conversation>
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
