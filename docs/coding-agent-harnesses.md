# Coding Agent Harnesses

The Codex, Claude Code, Cursor, and Opencode examples treat exoharness events as
canonical conversation state and run their native agent runtimes inside
configured exoharness sandboxes.

Install dependencies and build the CLI first:

```bash
pnpm install
cargo build -p exo
```

The examples below use `./target/debug/exo`. If you have the binary on your
`PATH`, you can use `exo` instead.

The `codex`, `claude-code`, `cursor`, and `opencode` harness presets select the
matching TypeScript module, sandbox image, and networking defaults.

For `secret set`, `--env` takes the variable name literally. For example, use
`--env OPENAI_API_KEY`, not `--env $OPENAI_API_KEY`.

The sandbox image commands below use Apple container, but it is not required.
The harness supports several sandbox providers (`SandboxProvider`), including
`docker`. The Containerfiles are plain OCI images, so any builder works:

- Apple container (default in these docs): `container build ...`, then run with
  `--sandbox-provider apple-container`.
- Docker: `docker build -f <Containerfile> -t <image> .`, then run with
  `--sandbox-provider docker`.

Pick the provider when creating the agent (or conversation) with
`--sandbox-provider <provider>`. To build the images during the live e2e with a
non-Apple builder, set `EXO_CONTAINER_CLI` (e.g. `EXO_CONTAINER_CLI=docker`).

Apple container currently requires an Apple silicon Mac running macOS 26 or
newer.

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
./target/debug/exo --harness codex agent create "TS Codex" \
  --model gpt-5.4

./target/debug/exo conversation create ts-codex
./target/debug/exo conversation mount add ts-codex <conversation> "$PWD" /workspace --rw
./target/debug/exo repl --agent ts-codex --conversation <conversation>
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
./target/debug/exo --harness claude-code agent create "TS Claude Code" \
  --model claude-sonnet-4-6

./target/debug/exo conversation create ts-claude-code
./target/debug/exo conversation mount add ts-claude-code <conversation> "$PWD" /workspace --rw
./target/debug/exo repl --agent ts-claude-code --conversation <conversation>
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
./target/debug/exo --harness cursor agent create "TS Cursor" \
  --model auto

./target/debug/exo conversation create ts-cursor
./target/debug/exo conversation mount add ts-cursor <conversation> "$PWD" /workspace --rw
./target/debug/exo repl --agent ts-cursor --conversation <conversation>
```

## Opencode

opencode is provider-agnostic. The harness runs the opencode server and SDK
client entirely inside the sandbox and bridges to exo over stdio (the same
in-sandbox worker pattern as Cursor), so no port forwarding is required. exo's
model binding (API key plus optional base URL) is applied as a provider override
inside the sandbox.

Register a model. The model name is an opencode `provider/model` reference; a
bare name falls back to the `anthropic` provider (override with
`EXO_OPENCODE_PROVIDER`):

```bash
./target/debug/exo secret set anthropic --env ANTHROPIC_API_KEY
./target/debug/exo model register anthropic/claude-sonnet-4-6 --secret anthropic
```

Build the sandbox image (it copies the monorepo so the in-sandbox worker can run
under tsx, and installs the `opencode` CLI that the SDK launches):

```bash
container build \
  --platform linux/arm64 \
  -f containers/opencode-sandbox/Containerfile \
  -t exo-opencode-sandbox:latest \
  .
```

Or with Docker:

```bash
docker build \
  -f containers/opencode-sandbox/Containerfile \
  -t exo-opencode-sandbox:latest \
  .
```

Create the agent and start a conversation (add `--sandbox-provider docker` if you
built with Docker):

```bash
./target/debug/exo --harness opencode agent create "TS Opencode" \
  --model anthropic/claude-sonnet-4-6

./target/debug/exo conversation create ts-opencode
./target/debug/exo conversation mount add ts-opencode <conversation> "$PWD" /workspace --rw
./target/debug/exo repl --agent ts-opencode --conversation <conversation>
```

## Live E2E

The live e2e script runs replay checks against the coding-agent harnesses:

```bash
pnpm e2e:agent-harnesses --only codex
pnpm e2e:agent-harnesses --only claude
pnpm e2e:agent-harnesses --only cursor
pnpm e2e:agent-harnesses --only opencode
```

Use `--build-images` to build the required sandbox images before running. The
build shells out to `EXO_CONTAINER_CLI` (default `container`); set
`EXO_CONTAINER_CLI=docker` to build with Docker instead.
