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

For `vault secret create`, `--token-env` takes the variable name literally. For example, use
`--token-env OPENAI_API_KEY`, not `--token-env $OPENAI_API_KEY`.

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
./target/debug/exo vault secret create global openai --token-env OPENAI_API_KEY --allow-origin https://api.openai.com
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
cat > ts-codex.md <<'EOF'
---
name: "TS Codex"
harness: codex
config:
  model: gpt-5.5
  credential: openai
---
Help the user with their task.
EOF
./target/debug/exo agent create ts-codex --file ts-codex.md

./target/debug/exo thread create ts-codex
./target/debug/exo thread mount create ts-codex <conversation> "$PWD" /workspace --rw
./target/debug/exo agent run --agent ts-codex --thread <conversation>
```

## Claude Code

Register an Anthropic model:

```bash
./target/debug/exo vault secret create global anthropic --token-env ANTHROPIC_API_KEY --allow-origin https://api.anthropic.com
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
cat > ts-claude-code.md <<'EOF'
---
name: "TS Claude Code"
harness: claude-code
config:
  model: claude-sonnet-4-6
  credential: anthropic
---
Help the user with their task.
EOF
./target/debug/exo agent create ts-claude-code --file ts-claude-code.md

./target/debug/exo thread create ts-claude-code
./target/debug/exo thread mount create ts-claude-code <conversation> "$PWD" /workspace --rw
./target/debug/exo agent run --agent ts-claude-code --thread <conversation>
```

## Cursor

Register a Cursor model:

```bash
./target/debug/exo vault secret create global cursor --token-env CURSOR_API_KEY
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
cat > ts-cursor.md <<'EOF'
---
name: "TS Cursor"
harness: cursor
config:
  model: auto
  credential: cursor
---
Help the user with their task.
EOF
./target/debug/exo agent create ts-cursor --file ts-cursor.md

./target/debug/exo thread create ts-cursor
./target/debug/exo thread mount create ts-cursor <conversation> "$PWD" /workspace --rw
./target/debug/exo agent run --agent ts-cursor --thread <conversation>
```

## Pi

Pi sandbox images must define `HOME` as a writable directory for session and tool files.

Register a model Pi supports. Pi reads the provider key from the sandbox
environment, so the same variable has to be set where exo runs:

```bash
./target/debug/exo vault secret create global openai --token-env OPENAI_API_KEY --allow-origin https://api.openai.com
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
cat > ts-pi.md <<'EOF'
---
name: "TS Pi"
harness: pi
config:
  model: gpt-5.5
  credential: openai
---
Help the user with their task.
EOF
./target/debug/exo agent create ts-pi --file ts-pi.md

./target/debug/exo thread create ts-pi
./target/debug/exo thread mount create ts-pi <conversation> "$PWD" /workspace --rw
./target/debug/exo agent run --agent ts-pi --thread <conversation>
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
