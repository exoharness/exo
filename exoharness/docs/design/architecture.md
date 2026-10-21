# Exoharness Architecture

This document describes the full Exoharness API from the perspective of the
subcomponents that use it. It covers the core Rust object model, the JSONL
protocol used to expose that model across process boundaries, the TypeScript
harness API, and the way the executor, managed adapters, and CLI compose those pieces.

## Mental Model

Exoharness is the durable runtime substrate for agents. It owns agents,
conversations, sessions, turns, events, artifacts, sandboxes, bindings, and
secrets. The executor layer sits on top and decides how a turn is run: a basic
LLM loop, an RLM loop, or a TypeScript harness. Managed agent definitions select instructions, tools, resources, and harness
defaults; adapters submit inputs and deliver the resulting execution stream.

The main boundary looks like this:

```text
CLI / HTTP API / managed adapters
  -> executor Harness facade
    -> exoharness object model
    -> executor turn runtime
      -> TypeScript runner, basic harness, or RLM harness
        -> tools, model calls, sandbox process APIs
```

The core API is intentionally small. Most operations are scoped to one of four
handle types:

- `ExoHarness`: root handle for global agents, bindings, and secrets.
- `AgentHandle`: per-agent conversations, artifacts, and attached vaults.
- `ConversationHandle`: per-conversation event log, artifacts, sandboxes,
  bindings, and secrets.
- `TurnHandle`: active-turn-only writes that must preserve turn consistency.

The distinction between `ConversationHandle` and `TurnHandle` is important.
Conversation-level writes append to the conversation head immediately. Turn-level
writes attach the active turn's session and turn ids to appended messages and
artifacts. Code that runs inside a turn should prefer `TurnHandle` for messages
and artifacts so the event log preserves turn ownership.

## Key Crates And Files

- `crates/exoharness/src/types.rs`: core Rust traits and data types.
- `crates/exoharness/src/basic.rs`: filesystem-backed implementation of the
  core traits.
- `crates/exoharness/src/protocol.rs`: JSON-serializable request and response
  protocol for the core API.
- `crates/exoharness/src/server.rs`: protocol server that dispatches JSON
  requests into an `ExoHarness`.
- `crates/executor/src/harness_types.rs`: higher-level executor-facing harness
  facade.
- `crates/executor/src/harness_executor.rs`: generic turn execution lifecycle.
- `crates/executor/src/runtime_host.rs`: task scheduling supplied by the deployment
  host, used by the shared provider and execution lifecycle.
- `crates/executor/src/native/`: native constructors, services and process adapters,
  compiled through the executor's default `native` feature.
- `crates/exoharness/src/http/`: state protocol client, HTTP transport and process
  streaming adapters; platform dependencies are gated within each module.
- `crates/exoharness/src/storage.rs`: byte storage interface for the shared store.
  Native object storage and Worker Durable Object/R2 adapters implement it.
- `crates/executor/src/managed_agents/service.rs`: shared managed-agent handlers
  used by native HTTP and Worker transports.
- `crates/executor/src/native/typescript.rs`: Rust host for TypeScript harness
  processes.
- `exoharness/typescript/harness/index.ts`: public TypeScript API exposed to harness
  authors.
- `exoharness/typescript/harness/runner.ts`: TypeScript guest process that converts the
  public TypeScript API into host protocol messages.

## Core Rust API

The core Rust API lives in the `exoharness` crate. Callers generally work with
trait objects (`Arc<dyn ExoHarness>`, `Arc<dyn AgentHandle>`,
`Arc<dyn ConversationHandle>`, and `Arc<dyn TurnHandle>`) rather than concrete
storage implementations.

### Root: `ExoHarness`

The root handle manages global state:

- `list_agents() -> Vec<AgentHandle>`
- `get_agent(id) -> Option<AgentHandle>`
- `new_agent(NewAgentRequest) -> AgentHandle`
- `delete_agent(id) -> bool`
- `list_bindings() -> Vec<BindingMetadata>`
- `put_binding(Binding) -> BindingId`
- `get_binding(id) -> Option<Binding>`
- `list_vaults() -> Vec<VaultHandle>`
- `get_vault(id) -> Option<VaultHandle>`
- `create_vault(name) -> VaultHandle`
- `delete_vault(id)`

Global bindings act as defaults. Vault access composes from global to agent to
thread through `VaultContext`. Creating a vault does not attach it to every agent.

### Vault: `VaultHandle`

A vault owns its secrets and authorizes their use:

- `record() -> VaultRecord`
- `list_secrets() -> Vec<SecretMetadata>`
- `put_secret(PutSecretRequest) -> SecretId`
- `get_secret(id) -> Option<Secret>`
- `update_secret(id, Secret) -> SecretMetadata`
- `delete_secret(id)`
- `resolve_secret(id, target) -> ResolvedSecret`

Agent and thread records store their attached vault ids. Bindings name a specific
vault and secret. MCP selects from the accessible vaults and saves those references
on the thread. Secret metadata includes a destination and revision for rotation.

### Agent: `AgentHandle`

An agent handle exposes conversations and agent-scoped resources:

- `record() -> AgentRecord`
- `list_conversations() -> Vec<ConversationHandle>`
- `get_conversation(id) -> Option<ConversationHandle>`
- `new_conversation(NewConversationRequest) -> ConversationHandle`
- `delete_conversation(id) -> bool`
- `list_bindings()`, `put_binding()`, `get_binding()`
- `list_vaults()`, `get_vault(id)`
- `list_artifacts()`, `write_artifact()`, `read_artifact()`

Agent-scoped artifacts are useful for configuration and data that should live
with the agent instead of one conversation.

### Conversation: `ConversationHandle`

A conversation handle owns the event log and the conversation-scoped execution
environment:

- `record() -> ConversationRecord`
- `start_session() -> SessionId`
- `end_session(session_id)`
- `begin_turn(BeginTurnRequest) -> TurnHandle`
- `get_events(EventQuery) -> GetEventsResult`
- `watch_events(after_exclusive) -> EventStream`
- `get_event(event_id) -> Option<Event>`
- `add_events(AddEventsRequest) -> AddEventsResult`
- `fork(ForkConversationRequest) -> ConversationHandle`
- `list_artifacts()`, `write_artifact()`, `read_artifact()`
- `create_sandbox(CreateSandboxRequest) -> SandboxId`
- `list_sandboxes() -> Vec<SandboxRecord>`
- `snapshot_sandbox(sandbox_id, kind) -> SnapshotId`
- `start_sandbox(StartSandboxRequest)`
- `stop_sandbox(sandbox_id)`
- `run_in_sandbox(RunInSandboxRequest) -> SandboxProcess`
- `list_bindings()`, `put_binding()`, `get_binding()`
- `list_vaults()`, `get_vault(id)`

Conversation-level `add_events()` is append-only. Callers that need to avoid
overlapping agent runs should serialize through the executor-level `send()` APIs
or another higher-level owner/lease.

### Turn: `TurnHandle`

A turn handle is the active-turn write surface:

- `record() -> TurnRecord`
- `add_events(Vec<EventData>) -> AddEventsResult`
- `write_artifact(WriteArtifactRequest) -> ArtifactVersion`
- `finish() -> EventId`

Turn writes append events tagged with the turn's session and turn ids.
`finish()` writes `turn_ended` and marks the handle complete. Calling
`finish()` again is idempotent and returns the existing finish event id.

## Records And IDs

Most persisted records use UUIDv7 IDs. These IDs carry sortable timestamps,
which makes event ordering straightforward.

Important records:

- `AgentRecord`: `id`, `slug`, `name`.
- `ConversationRecord`: `id`, `slug`, `name`, `latest_event_id`.
- `TurnRecord`: `id`, `session_id`.
- `Event`: `id`, `conversation_id`, optional `session_id`, optional `turn_id`,
  `created_at`, and tagged `data`.
- `ArtifactVersion`: `artifact_id`, `path`, `version`, `created_at`,
  `size_bytes`.

The public TypeScript API translates Rust snake_case fields into camelCase,
while the wire protocols preserve serde's snake_case tags and fields.

## Event Log API

The event log is the durable source of truth for conversation history and
runtime side effects. Events are append-only.

Core event variants:

- `conversation_forked`
- `session_started`
- `session_ended`
- `turn_started`
- `turn_ended`
- `messages`
- `tool_requested`
- `tool_result`
- `artifact_written`
- `sandbox_created`
- `sandbox_started`
- `sandbox_stopped`
- `sandbox_snapshotted`
- `custom`

`messages` events contain Lingua messages. `tool_requested` and `tool_result`
events preserve tool execution state across turns. `artifact_written` events
reference artifact metadata; artifact bytes live in artifact storage. `custom`
events are available for subcomponents that need typed event payloads without a
new core event variant.

Harnesses normalize their provider protocols into `messages`, `tool_requested`,
`tool_result`, and `lingua_stream_chunk`. Shared model-event helpers attach usage
and cost, including usage-only `messages` events for calls without assistant
text. Runtime routes active-turn writes through one event sink and publishes
canonical tool progress after persistence. Harnesses should append canonical
tool events; `context.stream.toolCall` and `toolResult` are for custom tool events
such as RLM's internal tools.

RLM retains custom events for its internal model responses and tools. Those
events stay outside conversation replay, and its model control text stays out
of the output stream. Only its final answer becomes a conversation message.

`EventQuery` supports:

- `cursor`: event id boundary.
- `direction`: `asc` or `desc`.
- `limit`: max number of events.
- `session_id`: filter to one session.
- `turn_id`: filter to one turn.
- `types`: filter by event type string.

`watch_events()` returns existing events after the requested bound and then live
events through an in-memory subscriber list. The basic implementation is useful
inside one process; it is not a cross-process pub/sub service.

## Sessions And Turns

A session groups related turns. `begin_turn()` requires a caller-allocated
`TurnRecord` containing the turn and session IDs. Set `new_session: true` to
append `session_started` before `turn_started`; use `false` when continuing an
existing session, including one opened with `start_session()`. If input messages
are included, they are appended as a `messages` event in the same turn.

The executor-level `send()` API starts a turn, executes the configured harness,
and finishes the turn. External systems such as adapters
usually call `send()` with a fresh session and then close the session after the
wakeup completes.

For code running during a turn, use `context.exoharness.current.turn` instead of
`context.exoharness.current.conversation` when appending messages or artifacts.
For code outside a turn, serialize wakeups per conversation if multiple external
sources can fire concurrently.

## Artifacts

Artifacts are versioned blobs addressed by logical path. Writing to the same
path creates a new version of the same artifact id. Writing to a new path
creates a new artifact id.

Artifact APIs exist at agent and conversation scope. Conversation-level
`write_artifact()` appends an `artifact_written` event to the conversation.
Turn-level `write_artifact()` also appends an `artifact_written` event, but it
tags the event with the active turn's session and turn ids.

TypeScript convenience methods:

- `writeArtifact({ path, contents })`
- `writeArtifactText({ path, text })`
- `writeArtifactJson({ path, value })`
- `readArtifact({ artifactId, version })`
- `readArtifactText(...)`
- `readArtifactJson<T>(...)`

The TypeScript tool registry writes full tool results to artifacts by default.
Large tool results are compacted in the conversation history while full
stdout/stderr/result JSON remains available under `tool-results/...`.

## Sandboxes

Sandboxes are conversation-scoped in the core API. A sandbox record stores:

- image
- default workdir
- filesystem mounts
- networking flag
- idle timeout
- running state
- latest snapshot id

The core methods create, start, stop, snapshot, and run processes in a sandbox.
`run_in_sandbox()` returns a `SandboxProcess` with async stdout, stderr, stdin,
and wait handles.

The executor adds policy on top:

- `ensure_conversation_sandbox()` creates or reuses the conversation sandbox.

From TypeScript, `context.startSandboxProcess({ command, env })` starts a
sandbox process and returns a `SandboxProcess` with `ReadableStream<string>`
stdout/stderr, `writeStdin()`, `closeStdin()`, `close()`, and `wait()`.

## Bindings And Secrets

Bindings and secrets exist at root, agent, and conversation scope.

Bindings:

- `env`: name, environment variable name, secret id.
- `mcp`: name, server URL, optional secret id.
- `llm`: name, model, optional base URL, optional secret id.

Secrets:

- `key`: raw API key or token.
- `oauth`: access token and optional refresh token.

The basic implementation encrypts secrets at rest through a configured secret
backend. Current backends include Apple Keychain, file-backed master key, and a
static key option for tests.

When listing bindings or secrets from agent or conversation scope, metadata is
merged by name. More local scopes override broader scopes.

## Core JSONL Protocol

The `exoharness::protocol` module exposes the core Rust API as a JSONL request
protocol. It is used by the TypeScript runner when TypeScript code calls
`context.exoharness`.

Every client message is:

```json
{ "kind": "request", "id": 1, "request": { "type": "list_agents" } }
```

Every server message is:

```json
{
  "kind": "response",
  "id": 1,
  "ok": true,
  "response": { "type": "agents", "agents": [] },
  "error": null
}
```

The protocol request variants mirror the core handles:

- Root: `list_agents`, `get_agent`, `new_agent`, `delete_agent`,
  `list_bindings`, `put_binding`, `get_binding`, `list_vaults`,
  `get_vault`, `create_vault`, `delete_vault`.
- Vault: `vault_list_secrets`, `vault_put_secret`, `vault_get_secret`,
  `vault_update_secret`, `vault_delete_secret`, `vault_resolve_secret`.
- Agent: `list_conversations`, `get_conversation`, `new_conversation`,
  `delete_conversation`, `agent_list_artifacts`, `agent_read_artifact`,
  `agent_write_artifact`, agent bindings.
- Conversation: `conversation_start_session`, `conversation_end_session`,
  `conversation_get_events`, `conversation_get_event`,
  `conversation_add_events`, `conversation_fork`,
  `conversation_list_artifacts`, `conversation_read_artifact`,
  `conversation_write_artifact`, conversation bindings.
- Turn: `turn_add_events`, `turn_write_artifact`, `turn_finish`.

Vault requests carry a `ResourceScope`: global, agent, or thread. Sandbox requests
use the same scope; snapshot and start operations also accept turn attribution.

The protocol addresses turns by durable ids. TypeScript code receives the active
turn's agent, conversation, session, and turn ids, and subsequent turn requests
send those ids back to the host.

## Executor Harness Facade

The executor crate wraps the core API with a product-facing `Harness` facade:

- `Harness`: list/create/delete agents, resolve agents by id or slug, flush
  tracing, and expose the underlying `ExoHarness`.
- `HarnessAgent`: read/write agent config, list/create/delete conversations,
  and expose the underlying `AgentHandle`.
- `HarnessConversation`: read/write conversation config, read/write model
  overrides, materialize messages, close sessions, send turns, stream turns, and
  expose the underlying `ConversationHandle`.

This facade adds configuration artifacts and model execution semantics to the
raw storage API. Callers that want to run the agent should use
`HarnessConversation::send()` or `send_stream()`. Callers that only need storage,
artifacts, events, or sandbox execution can use `exoharness_handle()`.

## Turn Execution Lifecycle

`Runtime` separates accepting a turn from executing it:

1. Authorize the request and capture its configuration and typed work payload.
2. Assign the turn and session IDs, enqueue the work, and return its receipt.
3. Claim the thread's queue and persist execution admission for its head.
4. Prepare the conversation, then call `begin_turn()` with the accepted IDs.
5. Run the selected executor and persist its terminal events.
6. Acknowledge the head and execute the next entry in order.

Acceptance does not wait for an earlier turn or VM startup. Dropping the progress
stream leaves accepted work queued; cancellation explicitly records a request in
the queue. Interruption records cancellation of the submitter's active turn and
appends its replacement atomically. Idempotency keys are scoped to the submitter;
the stored implementation keeps receipts for 24 hours after acceptance.

`TurnCoordinator<Work>` is a separate trait in `exoharness`. It defines ordering,
ownership, cancellation, suspension, and acknowledgment. Its `pending_threads()`
method supports startup discovery. A host that knows its thread can
call `LocalProvider::wake_turn_queue()` after a wakeup without global discovery.
Timed ownership renewal and worker scheduling belong to the deployment backend.

`Runtime::suspend_turn()` persists a pause and stops execution without ending the
turn. A suspended head holds queue order across restart. `Runtime::resume_turn()`
returns a new progress stream and recovers the same turn and session IDs;
existing observers receive one nonterminal `Suspended` event. `send()` and the
TUI stop waiting at suspension; stream observers may stay attached for resume.
Cancellation ends a suspended turn. Executors that cannot reconcile unresolved
tools defer suspension until their outstanding tool requests have results,
including approvals. Unsupported suspension returns `false` without saving a
pause. Resuming during that wait clears the pending suspension.
The coordinator's control watch delivers changes during execution, including
writes from another backend worker.

The CLI explicitly supplies `BasicExoHarness::turn_coordinator()`, which persists
`turn_queue.json` in each thread's directory. Embedded `LocalProvider` constructors
use a volatile queue with no recovery across process restarts. Durable recovery
requires a persistent coordinator supplied through `with_turn_coordinator()`. The
stored implementation requires one host owner; Basic's local session leases
enforce this across processes and separately protect thread mutations and VMs.

Durable Object hosts must explicitly install a persistent coordinator with
`with_turn_coordinator()` and schedule their alarm before reporting acceptance.
An account object serving several threads needs discovery; a host given a thread
ID can wake that thread directly.

On restart, the runtime reconciles the saved queue head with its recovery journal.
It retains the accepted ID if execution never started, resumes unfinished work,
and acknowledges completed work without running it again. The queue is the sole
source of pending work; new turns queue behind it without waiting for a startup
scan. Sandbox ownership and shutdown remain with the state implementation.

A competing-worker coordinator must implement atomic storage mutations and
report lost ownership through the control watch when renewal fails; the runtime
then stops execution. Queue ownership does not itself fence event or tool writes,
and recovery does not guarantee exactly-once external side effects.

The actual turn runner is a `HarnessExecutor` implementation. Current executor
implementations include the basic harness, RLM harness, and TypeScript harness.

Streaming turns return an `ExecutionStreamHandle` and emit:

- first-chunk timing
- text deltas
- tool calls
- tool results
- final completion or error

## TypeScript Harness Host Protocol

The TypeScript executor runs one persistent Node process per harness module
path. The host command is:

```text
node --import tsx exoharness/typescript/harness/runner.ts <modulePath>
```

Rust writes host-to-guest JSONL messages to stdin. TypeScript writes
guest-to-host JSONL messages to stdout. Stderr is captured for error reporting.
Runner processes are cached by module path and removed from the cache if a turn
fails.

Host-to-guest messages:

- `init`: starts a turn and includes agent, conversation, turn, configs, request,
  streaming flag, and optional tracing parent.
- `shutdown`: closes the runner loop.
- `runtime_response`: response to a runtime request.
- `exo_response`: response to a core Exoharness API request.
- `runtime_event`: async sandbox process output, exit, or error.

Guest-to-host messages:

- `runtime_request`: asks Rust to execute a tool or manage a sandbox process.
- `exo_request`: asks Rust to perform a core Exoharness protocol request.
- `stream_event`: forwards stream output to the executor stream.
- `done`: marks the TypeScript turn complete.
- `error`: fails the turn with message and stack.

The TypeScript runner imports the harness module once, then loops over `init`
messages. A module must export either a default harness or a named `harness`
export with `runTurn(context)`.

## TypeScript Public API

The public TypeScript API is exported from `@exo/harness`.

A harness module is:

```ts
import { defineHarness } from "@exo/harness";

export default defineHarness({
  async runTurn(context) {
    // Run one turn.
  },
});
```

`TurnContext` exposes:

- `agentConfig`: model, instructions, TypeScript module config, tool settings,
  sandbox image, networking, token and tool round-trip limits, and tracing
  config.
- `conversationConfig`: networking, shell program, sandbox scope, and mounts.
- `request`: input messages and optional session id.
- `streaming`: whether stream events should be emitted.
- `braintrustParent`: optional tracing parent id.
- `exoharness`: current and global Exoharness API.
- `executeTool(request)`: ask the Rust tool runtime to execute a configured
  tool.
- `startSandboxProcess(request)`: start an interactive sandbox process.
- `executePendingTools(toolCalls)`: convenience for sequential tool execution.
- `stream`: first chunk, text, tool-call, and tool-result stream emitters.

`context.exoharness.current` provides the current:

- `agent`
- `conversation`
- `turn`

The `Agent`, `Conversation`, and `Turn` TypeScript objects intentionally expose
the subset of the Rust handles needed by harness code, with camelCase fields and
convenience methods for text and JSON artifacts. Bindings and secrets are
readable from TypeScript, but binding/secret mutation remains on the Rust core
protocol and executor configuration paths rather than the public TypeScript
harness API.

Helpers in `@exo/harness` include:

- `messagesEvent()`, `toolRequestedEvent()`, `toolResultEvent()`
- `appendMessages()`, `appendCustomEvent()`, `replyText()`
- `getMessages()`, `materializeConversationMessages()`
- `materializePromptMessages()`
- `messagesToHistoryMessages()`, `messagesToTranscript()`
- `projectAnthropicMessageToolEvents()`
- `assertRoundBudget()`
- `turnMetadata()`

## Tool API

Tools are represented as `ToolInstance` values:

- `definition`: name, description, JSON-schema parameters, optional output
  schema.
- `source`: `built_in`, `library`, or `agent`.
- `handler.execute(args, execution)`: implementation.

Reusable tools can be authored with `defineTool()`. Tool modules can be loaded
from library paths or from agent-created tool directories.

Tool definitions are validated:

- `name` must be non-empty, at most 64 characters, and only contain letters,
  numbers, underscores, and dashes.
- `parameters` must be an object JSON schema.
- `parameters.additionalProperties` must be `false`.
- Handlers must implement `execute`, not `invoke`.

Tool execution flow:

1. The harness registers tools in a `HarnessToolRegistry`.
2. The model requests one or more tool calls.
3. The harness appends `tool_requested` events through the active turn.
4. `executePendingTools()` or the registry executes tools.
5. Tool results are normalized, streamed if needed, compacted into artifacts,
   and appended as `tool_result` events.

Large tool results are not kept fully inline in the message history. The compact
result contains preview text, truncation metadata, and artifact references.

## Model Loop Integration

The TypeScript harness API does not directly prescribe a model loop. Exo and
the basic TypeScript harness use shared turn-loop helpers to:

1. Materialize prompt messages from configured instructions and conversation
   events.
2. Register built-in, library, and agent-created tools.
3. Call the model.
4. Project model text and tool calls into Exoharness events.
5. Execute tools and continue until the model finishes or the configured tool
   round-trip budget is reached.

The Rust model runtime uses Lingua's universal message and tool abstractions and
routes requests through `braintrust_llm_router`.

## Self-improving agent example

The [self-improving agent](../../../examples/self-improving-agent/README.md)
composes the TypeScript turn loop with its own tools, prompts, scheduler, and
guardian scripts. It currently uses the native executor's `ExoToolRuntime` and
scheduler support. Its implementation lives under `examples/self-improving-agent/`.

## Managed Adapters

Managed adapters live in `crates/executor/src/managed_agents/adapters/`. Slack
supports Socket Mode and HTTP callbacks. The adapter maps a Slack conversation to
an Exo thread, submits each input to the normal turn queue, and renders its execution
stream. Durable inbox, outbox, and approval records preserve admission and delivery
across restarts. See [Managed adapters](../../../docs/managed-adapters.md) for
identity, admission, recovery, and delivery details.

## CLI Integration

The CLI talks to the executor runtime to create agents, open threads, submit and
stream turns, inspect events, and manage environments and vaults. `exo run AGENT`
opens a chat; `exo run AGENT --serve` serves that agent's API and adapters, while
`exo run --serve` serves the provider's agents.

## Storage Implementation

`BasicExoHarness` is the current local implementation. It stores records and
artifacts under a filesystem-backed object store rooted at the configured
harness root.

Important properties:

- A single async write lock serializes writes inside one process.
- Event files are sorted by UUIDv7 event id.
- Conversation records cache `latest_event_id`.
- Subscribers are in-memory and per-process.
- Secrets are encrypted before writing.
- Artifact metadata and bytes are written separately.
- Sandboxes are tracked in metadata plus an in-memory running-sandbox table.

Because some coordination is in-memory, multiple independent processes writing
to the same harness root should be treated carefully. The basic backend is
primarily designed for a coordinated local runtime.

## Concurrency Rules

Use these rules when adding new subcomponents:

- If code is running inside a turn, append messages and artifacts through
  `TurnHandle` or `context.exoharness.current.turn`.
- If code is outside a turn and wants the agent to react, call the executor
  `send()` API rather than appending assistant messages directly.
- If multiple external sources can wake the same conversation, serialize those
  wakeups.
- Do not write conversation artifacts from adapter plumbing while a
  wakeup turn is active.
- Keep durable subsystem queues in their subsystem stores when the data is not
  part of the agent-visible conversation history.

## Extending Exoharness

Add to the core `exoharness` API only for durable, generic agent runtime
concepts. Good candidates are things every harness implementation should support
or expose consistently, such as conversations, events, artifacts, and sandboxes.

Prefer executor- or product-level extensions when the feature is specific to a
runtime or application:

- Managed adapters live in executor code because they connect external
  applications to the runtime's turn and delivery lifecycle.
- Tool definitions live in TypeScript and executor tool runtimes because they
  are model-facing behavior, not storage primitives.

When adding a new core operation, update all three surfaces:

1. Rust traits and implementation.
2. `exoharness::protocol` request/response variants and server dispatch.
3. TypeScript raw protocol types plus public wrapper methods.

When adding a TypeScript-only runtime capability, update both sides of the
TypeScript harness protocol:

1. `crates/executor/src/native/typescript.rs` host message/request/response handling.
2. `exoharness/typescript/harness/runner.ts` raw types and `TurnContext` implementation.
3. `exoharness/typescript/harness/index.ts` public type definitions.

## Common Call Flows

### User Sends A CLI Message

```text
CLI
  -> HarnessConversation::send()
    -> ExecutorHarnessRuntime::send()
      -> ConversationHandle::begin_turn()
      -> selected executor runs turn
      -> TurnHandle::add_events(...)
      -> TurnHandle::finish()
```

### TypeScript Harness Calls A Core API

```text
harness module
  -> context.exoharness.current.conversation.getEvents(...)
    -> runner.ts sends { kind: "exo_request" }
      -> TypeScriptRunnerProcess receives request
        -> ExoHarnessServer::handle_request()
          -> ConversationHandle::get_events()
        -> host sends { kind: "exo_response" }
      -> runner resolves TypeScript promise
```

### TypeScript Harness Executes A Tool

```text
harness module
  -> context.executeTool({ functionName, arguments })
    -> runner.ts sends { kind: "runtime_request", type: "execute_tool" }
      -> TypeScriptRunnerProcess calls ToolRuntime::execute()
      -> host sends runtime_response tool_result
    -> harness appends tool_result event through TurnHandle
```

### TypeScript Harness Starts A Sandbox Process

```text
harness module
  -> context.startSandboxProcess({ command, env })
    -> runner.ts sends runtime_request start_sandbox_process
      -> Rust ensures conversation sandbox
      -> ConversationHandle::run_in_sandbox()
      -> host returns process_id
      -> host streams runtime_event sandbox_process_output/exit/error
    -> TypeScript SandboxProcess exposes streams and wait()
```

### Adapter Message Starts A Turn

```text
Slack Socket Mode / HTTP callback
  -> admission and durable inbox receipt
    -> resolve the conversation's Exo thread and sender's execution context
      -> Runtime::start_turn()
        -> normal per-thread turn queue and selected harness
      -> render the execution stream into the durable outbox
        -> deliver progress, approvals, and the final answer to Slack
```

## Current Caveats

- The basic backend's event subscribers and running sandbox table are
  in-memory, so they are not a distributed coordination layer.
- TypeScript turn requests carry durable turn ids, but there is no separate
  turn lease token. Coordinate concurrent writers at the executor/wakeup layer.
- Tool results are compacted for model history, so callers should follow
  artifact references for full output.
- TypeScript runner protocol messages are line-delimited JSON. Anything written
  to stdout by the runner process must be protocol JSON; diagnostics should go
  to stderr.
- Scheduler and adapters are users of Exoharness, not core Exoharness features.
  Their durable stores are separate from the core event log.
