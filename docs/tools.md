# Tools

The TypeScript harness has broad support for tool use. There are basic tools
provided by the harness, as well as agent generated tools, and support for tools
developed by the user or a third party. Most generally, tools are model-callable
functions. They let a model ask the harness to do something outside the model
call, such as run a shell command, call a service, inspect local state, or
execute project-specific code.

## Tool Types

### Built-In Tools

Built-in tools are maintained by the `exo` project and shipped with the harness
runtime.

The basic TypeScript harness exposes `shell` and, when agent tool creation is
enabled, `install_agent_tool`. The shell tool runs commands in the conversation
sandbox, using the conversation's configured `shellProgram`. Execution is
delegated to the Rust host runtime so sandbox lifecycle and process execution
remain host-managed.

`install_agent_tool` writes an agent-created TypeScript tool module into
`.exo/agent-tools/` and validates it. The basic harness refreshes tools before
each model round, so an installed tool can be used later in the same user turn.

If a tool throws during execution or installation, the registry records a
`tool_result` error instead of crashing the turn. If a previous process crash
left a tool request without a result, prompt materialization synthesizes an error
result so the conversation can continue.

### Library Tools

Library tools are not written by the agent itself and are not part of the core
`exo` release. They may be written by the user, a team, or an external
maintainer. They are loaded explicitly by the harness.

A library tool is a TypeScript module exporting a `Tool`, `ToolModuleEntry`,
`ToolModule`, or array of those values. The core `Tool` contract is:

- `definition.name` is the model-facing tool name.
- `definition.parameters` is a strict JSON object schema with
  `additionalProperties: false`.
- `initializationParameters` validates module-provided configuration.
- `initialize(...)` returns a handler with `execute(args, execution)`.

Use a `ToolModuleEntry` when a reusable tool needs configuration values:

```ts
export default {
  tool: uppercaseTool,
  initialization: { prefix: "UPPER: " },
} satisfies ToolModuleEntry;
```

Tools should not use `inputSchema`, `call`, or `invoke`; those are not part of
the `exo` tool contract.

### Agent Tools

Agent tools are created by the agent itself. They use the same default-export
`Tool` module contract as library tools, but they are registered with source
`"agent"` instead of `"library"`.

Agent tools should be treated as less trusted than built-in or library tools.
The basic harness loads them from `.exo/agent-tools/` when agent tool creation
is enabled. That setting is enabled by default and can be disabled per agent.

The loader:

- imports the module's default export
- verifies it looks like a `Tool`
- validates `initialization` against `initializationParameters`
- initializes the tool with source `"agent"`
- registers the resulting `ToolInstance`

Agent tool directory loading scans `.ts` files and ignores `.source.ts` files,
which keep the original generated source beside the validated wrapper.

## Events

Tool use is stored in the conversation event log.

The model runtime records tool requests as `tool_requested` events. The registry
returns `tool_result` events after execution. The harness appends those events
to the current turn.

This means the durable conversation history records:

1. The model requested a tool.
2. The harness executed or rejected it.
3. The result was returned to the next model round.

Tracing can separately record richer information, such as duration, errors, or
tool source, without changing the canonical event shape.

Tool results are artifact-backed. The registry writes the full tool result to a
conversation artifact and returns a compact model-facing result containing:

- artifact metadata for the full result
- a small preview
- the inline value only when the serialized result is small enough

For shell-like results, non-empty `stdout` and `stderr` are also written as
separate text artifacts. This keeps large HTML pages, logs, browser output, and
data dumps out of the model context while preserving the complete data for later
inspection or targeted reads.

## Configuration And Secrets

Tools should use existing exoharness configuration primitives:

- Put credentials in `Secret`.
- Refer to secrets by id in initialization parameters.
- Keep non-secret setup values in harness code, config, module exports, or
  artifacts.
- Do not put raw secrets in tool definitions, model-visible prompts, or
  `tool_result` events.

For example, an IRC tool might take `passwordSecretId` as an initialization
parameter. The tool handler can resolve the secret at execution time through the
exoharness API.

## Command Line Loading

TypeScript agents can load library tools from TypeScript tool modules when the
agent is created or updated:

```bash
exo --harness typescript agent create "Tool Demo" \
  --module examples/typescript/basic-harness.ts \
  --model gpt-5.4 \
  --tool-module examples/typescript/tools/uppercase.ts
```

`--tool-module` may be passed more than once. Each value is a TypeScript module
path. The module can default-export a `Tool`, `ToolModuleEntry`, `ToolModule`,
or an array of those values.

Existing TypeScript agents can be updated in place:

```bash
exo agent update tool-demo \
  --tool-module examples/typescript/tools/uppercase.ts

exo agent update tool-demo --clear-tool-modules
```

Agent tool creation is enabled by default. Disable or re-enable it with:

```bash
exo --harness typescript agent create "Locked Down" \
  --module examples/typescript/basic-harness.ts \
  --model gpt-5.4 \
  --tool-creation disabled

exo agent update demo --tool-creation disabled
exo agent update demo --tool-creation enabled
```

## Safety Considerations

Different tool sources have different trust levels:

- Built-in tools are first-party and reviewed with `exo`.
- Library tools are trusted by the user or harness author who chose to load
  them.
- Agent tools are generated by the agent and should have the narrowest scope.

Recommended defaults:

- Load tools explicitly, not by scanning directories.
- Validate initialization parameters before exposing a tool.
- Validate generated tools against the `Tool` contract before adding them to the
  manifest.
- Keep `tool_result` payloads compact; full tool outputs should flow through
  artifacts.
- Require explicit networking enablement for tools that call external services.
- Require confirmation for tools with external side effects.
- Avoid persisting agent tools beyond the conversation or workspace unless a
  user reviews and promotes them.
- Keep large logs in artifacts, not event payloads.

Agent tools currently run in the TypeScript harness process. They can use Node
built-ins, global APIs such as `fetch`, and dependencies already available to
the harness. Dependencies installed inside the conversation sandbox are not
automatically available to host-loaded agent tools; tools that need sandbox
state should call sandbox APIs from their handler.

## Current Status

The generic registry, built-in tool registration, library tool module loading,
and agent tool directory loading are implemented in the TypeScript harness API.

The basic TypeScript harness currently opts into `shell`, library tool modules
stored on the agent config, and agent-created tools from `.exo/agent-tools/`
when agent tool creation is enabled.

There is an example library tool at `examples/typescript/tools/uppercase.ts`.
It exists to test and demonstrate the registry contract, and can be enabled with
`--tool-module examples/typescript/tools/uppercase.ts`.
