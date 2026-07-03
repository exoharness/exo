# exo web — API reference

Typed client and protocol surface used by exo web. For product context see [`README.md`](../README.md); for design rationale see [`DESIGN.md`](DESIGN.md).

---

## Transport

### Substrate health

`GET {base}/health` → plain text (typically `ok`).

`ExoClient.health()` calls this endpoint directly (not `POST /request`).

### Substrate requests

`POST {base}/request` with a JSON envelope:

```json
{
  "kind": "request",
  "id": 1,
  "request": { "type": "list_agents" }
}
```

Response envelope (`ServerMessage`):

```json
{
  "kind": "response",
  "id": 1,
  "ok": true,
  "response": { "type": "agents", "agents": [] },
  "error": null
}
```

`ExoClient` increments `id` per call, validates `kind`, `id`, `ok`, and that `response.type` matches the expected variant.

### URL normalization

| Export                              | Purpose                                                                                                      |
| ----------------------------------- | ------------------------------------------------------------------------------------------------------------ |
| `normalizeRequestEndpoint(baseUrl)` | Resolves relative paths against `window.location.origin`, strips query/hash, appends `/request` when missing |
| `normalizeHealthEndpoint(baseUrl)`  | Same base resolution; maps `/request` suffix to sibling `/health`                                            |

Default dev base URL is `/exo` (Vite proxy → substrate on `127.0.0.1:4766`).

### Chat bridge (write path)

Not part of `ExoClient`. The UI posts to same-origin `POST /chat`:

**Request body**

```json
{
  "agent": "<agent_id>",
  "conversation": "<conversation_id>",
  "message": "<text>"
}
```

**Success (`200`)**

```json
{
  "ok": true,
  "exit_code": 0,
  "stderr": null,
  "latest_event_id": "<event_id>|null"
}
```

**Failure (`400` / `500`)**

```json
{ "ok": false, "error": "<message>", "exit_code": <number|null>, "stderr": "<preview>|null" }
```

Bridge health: `GET /health` → `ok`.

---

## `ExoClient`

```ts
new ExoClient(baseUrl: string)
```

Constructs a client with normalized request/health endpoints and an in-memory artifact cache (keyed by `agent:conversation:artifactId:version`).

### Constants

| Name                          | Value | Use                                          |
| ----------------------------- | ----- | -------------------------------------------- |
| `EVENT_PAGE_SIZE`             | `500` | Page size for `getEventsPage`                |
| (private) `DEFAULT_PAGE_SIZE` | `100` | Page size for `listConversations` pagination |

---

### `health(signal?: AbortSignal): Promise<string>`

**Purpose:** Substrate liveness check for the top-bar health badge.

**Request:** `GET` on normalized health endpoint.

**Returns:** Response body as plain text.

**Errors:** Non-OK HTTP status.

---

### `listAgents(): Promise<AgentRecord[]>`

**Purpose:** Populate the agent list in the left nav.

**Protocol request:**

```ts
{
  type: "list_agents";
}
```

**Protocol response:** `{ type: "agents", agents: AgentRecord[] }`

**Returns:** `agents` array.

**`AgentRecord`:** `{ id, slug, name }`

---

### `listConversations(agentId): Promise<ConversationHandleInfo[]>`

**Purpose:** List all conversations for an agent (auto-paginates).

**Args:** `agentId: AgentId`

**Protocol request (per page):**

```ts
{
  type: "list_conversations",
  agent_id: agentId,
  request: { cursor: EventId | null, limit: 100 }
}
```

**Protocol response:** `{ type: "conversations", result: ListConversationsResult<ConversationHandleInfo> }`

**Returns:** Concatenated `result.conversations` until `result.next_cursor` is null.

**`ConversationHandleInfo`:** `{ agent_id, record: ConversationRecord }`

**`ConversationRecord`:** `{ id, slug, name, latest_event_id }`

---

### `getEventsPage(agentId, conversationId, cursor): Promise<GetEventsResult>`

**Purpose:** Fetch one page of conversation events strictly after `cursor`, ascending. Used by `useConversationEvents` for initial load and polling.

**Args:**

- `agentId: AgentId`
- `conversationId: ConversationId`
- `cursor: EventId | null` — last seen event id; `null` for from-start

**Protocol request:**

```ts
{
  type: "conversation_get_events",
  agent_id: agentId,
  conversation_id: conversationId,
  query: { cursor, direction: "asc", limit: 500 }
}
```

**Protocol response:** `{ type: "events", result: GetEventsResult }`

**Returns:**

```ts
{
  events: Event[];
  cursor: EventId | null;  // substrate cursor for this page
}
```

**`Event`:** `{ id, conversation_id, session_id, turn_id, created_at, data: EventData }`

Event types rendered or derived in the UI include `messages`, `tool_requested`, `tool_result`, `artifact_written`, session/turn lifecycle, sandbox events, `error`, `custom`, and others defined in `EventData`.

---

### `readConversationArtifact(agentId, conversationId, artifactId, version?): Promise<Artifact>`

**Purpose:** Lazy-load artifact bytes for the artifact viewer. Results are cached for the client instance.

**Args:**

- `agentId`, `conversationId`, `artifactId`
- `version?: number | null` — omitted or `null` means latest

**Protocol request:**

```ts
{
  type: "conversation_read_artifact",
  agent_id: agentId,
  conversation_id: conversationId,
  request: { artifact_id: artifactId, version: version ?? null }
}
```

**Protocol response:** `{ type: "artifact", artifact: Artifact | null }`

**Returns:** `Artifact` or throws if missing.

**`Artifact`:** `{ artifact_id, path, version, created_at, size_bytes, contents: number[] }`

---

### `listRootSecrets(): Promise<SecretMetadata[]>`

**Purpose:** Global-scope secrets for the state panel.

**Protocol request:** `{ type: "list_secrets" }`

**Protocol response:** `{ type: "secrets", secrets: SecretMetadata[] }`

---

### `listAgentSecrets(agentId): Promise<SecretMetadata[]>`

**Protocol request:** `{ type: "agent_list_secrets", agent_id: agentId }`

**Protocol response:** `{ type: "secrets", secrets: SecretMetadata[] }`

---

### `listConversationSecrets(agentId, conversationId): Promise<SecretMetadata[]>`

**Protocol request:**

```ts
{
  type: "conversation_list_secrets",
  agent_id: agentId,
  conversation_id: conversationId
}
```

**Protocol response:** `{ type: "secrets", secrets: SecretMetadata[] }`

**`SecretMetadata`:** `{ id, type: "key" | "oauth", name, created_at }` — values are never fetched.

---

### `listRootBindings(): Promise<BindingRecord[]>`

**Protocol request:** `{ type: "list_bindings" }`

**Protocol response:** `{ type: "bindings", bindings: BindingRecord[] }`

---

### `listAgentBindings(agentId): Promise<BindingRecord[]>`

**Protocol request:** `{ type: "agent_list_bindings", agent_id: agentId }`

**Protocol response:** `{ type: "bindings", bindings: BindingRecord[] }`

---

### `listConversationBindings(agentId, conversationId): Promise<BindingRecord[]>`

**Protocol request:**

```ts
{
  type: "conversation_list_bindings",
  agent_id: agentId,
  conversation_id: conversationId
}
```

**Protocol response:** `{ type: "bindings", bindings: BindingRecord[] }`

**`BindingRecord`:** `{ id, type: "env" | "mcp" | "llm" | "sandbox", name, created_at, binding: Binding }`

Binding payloads vary by `type` (`env`, `mcp`, `llm`, `sandbox`); `secret_id` fields are shown as short ids in the UI, not resolved to values.

---

## `ReadOnlyRequest` vs full `ExoRequest`

`protocol.ts` defines the full `ExoRequest` union (agents, conversations, turns, artifacts, sandbox mutations, secrets, bindings, etc.).

`ExoClient` only sends variants in `ReadOnlyRequest`:

| Used by exo web                                                      | Not used by exo web                                                                                  |
| -------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------- |
| `list_agents`                                                        | `get_agent`, `new_agent`, `delete_agent`                                                             |
| `list_conversations`                                                 | `get_conversation`, `new_conversation`, `delete_conversation`, `conversation_fork`                   |
| `conversation_get_events`                                            | `conversation_get_event`, `conversation_add_events`, `conversation_begin_turn`, session/turn helpers |
| `conversation_read_artifact`                                         | `conversation_write_artifact`, `conversation_list_artifacts`, agent-scoped artifact ops              |
| `list_secrets`, `agent_list_secrets`, `conversation_list_secrets`    | `get_secret`, `put_secret`, agent/conversation put/get                                               |
| `list_bindings`, `agent_list_bindings`, `conversation_list_bindings` | `put_binding`, `get_binding`, scoped put/get                                                         |
| —                                                                    | All `conversation_create_sandbox`, process I/O, wait/cancel APIs                                     |
| —                                                                    | `conversation_get_sandbox_process_events` (in `ReadOnlyRequest` type but not called)                 |

Sandbox panels are built by folding `EventData` in `deriveSandboxState`, not via live sandbox APIs.

---

## Event data the UI consumes

| `EventData.type`                         | UI behavior                                                      |
| ---------------------------------------- | ---------------------------------------------------------------- |
| `messages`                               | User/assistant bubbles, reasoning blocks, usage metrics, rollups |
| `tool_requested` / `tool_result`         | Paired tool cards; orphan results shown separately               |
| `artifact_written`                       | Artifact card with lazy viewer                                   |
| `session_*`, `turn_*`, `conversation_*`  | System events (toggleable)                                       |
| `sandbox_*`                              | Derived into sandbox state panel                                 |
| `error`, `custom`, `lingua_stream_chunk` | Rendered per transcript rules                                    |

---

## Related modules (not `ExoClient`)

| Module                      | Role                                                                        |
| --------------------------- | --------------------------------------------------------------------------- |
| `useConversationEvents`     | Cursor polling over `getEventsPage`; exposes `poll()` for post-chat refresh |
| `App.sendChatTurn`          | `fetch("/chat", …)` wrapper                                                 |
| `ArtifactContext`           | Injects `readConversationArtifact` loader into transcript/viewer            |
| `deriveSandboxState`        | Pure fold over events → `SandboxView[]`                                     |
| `computeConversationRollup` | Token/cost/duration aggregates from `messages` + `usage`                    |
