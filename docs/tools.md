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
`.exo/agent-tools/`, validates it, and updates `.exo/agent-tools/manifest.json`.
The basic harness refreshes tools before each model round, so an installed tool
can be used later in the same user turn.

If a tool throws during execution or installation, the registry records a
`tool_result` error instead of crashing the turn. If a previous process crash
left a tool request without a result, prompt materialization synthesizes an error
result so the conversation can continue.

### Library Tools

Library tools are not written by the agent itself and are not part of the core
`exo` release. They may be written by the user, a team, or an external
maintainer. They are loaded explicitly by the harness.

A library tool is a TypeScript module with a default export satisfying `Tool`:

- `definition.name` is the model-facing tool name.
- `definition.parameters` is a strict JSON object schema with
  `additionalProperties: false`.
- `initializationParameters` validates manifest-time configuration.
- `initialize(...)` returns a handler with `execute(args, execution)`.

Tools should not use `inputSchema`, `call`, or `invoke`; those are not part of
the `exo` tool contract.

### Agent Tools

Agent tools are created by the agent itself. They use the same default-export
`Tool` module contract as library tools, but they are registered with source
`"agent"` instead of `"library"`.

Agent tools should be treated as less trusted than built-in or library tools.
The basic harness loads them from `.exo/agent-tools/manifest.json` when agent
tool creation is enabled. That setting is enabled by default and can be disabled
per agent.

The loader:

- imports the module's default export
- verifies it looks like a `Tool`
- validates `initialization` against `initializationParameters`
- initializes the tool with source `"agent"`
- registers the resulting `ToolInstance`

No directory scanning is done by default. The manifest is explicit so users and
harness authors can see which agent-created modules are being loaded.

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

## Configuration And Secrets

Tools should use existing exoharness configuration primitives:

- Put credentials in `Secret`.
- Refer to secrets by id in initialization parameters.
- Keep non-secret setup values in harness code, config, manifests, or artifacts.
- Do not put raw secrets in tool definitions, model-visible prompts, or
  `tool_result` events.

For example, an IRC tool might take `passwordSecretId` as an initialization
parameter. The tool handler can resolve the secret at execution time through the
exoharness API.

## Command Line Loading

TypeScript agents can load library tools from manifest files when the agent is
created or updated:

```bash
exo --harness typescript agent create "Tool Demo" \
  --module examples/typescript/basic-harness.ts \
  --model gpt-5.4 \
  --tool-manifest examples/typescript/tools/uppercase.manifest.json
```

`--tool-manifest` may be passed more than once. Relative `modulePath` values in
each manifest are resolved relative to that manifest file.

Agent tool creation is enabled by default. Disable or re-enable it with:

```bash
exo agent create "Locked Down" \
  --model gpt-5.4 \
  --disable-agent-tool-creation

exo agent update demo --disable-agent-tool-creation
exo agent update demo --enable-agent-tool-creation
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

The generic registry, built-in tool registration, library tool loading, and
agent tool manifest loading are implemented in the TypeScript harness API.

The basic TypeScript harness currently opts into `shell`, library tool manifests
stored on the agent config, and agent-created tools from
`.exo/agent-tools/manifest.json` when agent tool creation is enabled.

There is an example library tool at `examples/typescript/tools/uppercase.ts`.
It exists to test and demonstrate the registry contract, and can be enabled with
`examples/typescript/tools/uppercase.manifest.json`.
