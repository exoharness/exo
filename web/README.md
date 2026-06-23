# exo web

A local browser UI for inspecting an exo runtime and chatting with an agent. The inspector is **read-only** against the exo substrate HTTP API; the only write path is a separate local chat bridge that shells the `exo` CLI.

## Screenshots

<!-- Add screenshots here when available. Suggested captures:
  - Full workspace (left nav, transcript, state panel)
  - Tool-call card with expanded arguments/result
  - Artifact viewer (markdown / image)
  - Dark theme toggle
-->

| View               |                      |
| ------------------ | -------------------- |
| Workspace          | _screenshot pending_ |
| Transcript + tools | _screenshot pending_ |
| State panel        | _screenshot pending_ |

## What it does

exo web connects to a running **exoharness** HTTP server and shows:

- **Agents and conversations** — browse, filter, and select.
- **Live transcript** — messages, tool calls/results, system events, usage metrics; polls for new events while a turn runs.
- **State inspector** — secrets (metadata only), bindings, and sandbox state reconstructed from conversation events.
- **Artifact viewer** — read-only preview of conversation artifacts (text, JSON, markdown, images).
- **Chat composer** — send a user message via the local bridge; the turn runs in the CLI and events appear through the same poll loop.
- **New conversation** — the `+ New` control in the sidebar creates a conversation through the same bridge (`exo conversation create`), then selects it.

The top bar shows substrate health (`GET /health`), a configurable base URL, and a **read-only inspector** label. Chat is optional and requires the bridge.

## Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│  Browser (Vite dev server)                                      │
│  ┌──────────────┐  POST /exo/request   ┌─────────────────────┐  │
│  │  ExoClient   │ ───────────────────► │  exo substrate      │  │
│  │  (read-only) │  GET  /exo/health    │  127.0.0.1:4766     │  │
│  └──────────────┘                      └─────────────────────┘  │
│  ┌──────────────┐  POST /chat           ┌─────────────────────┐  │
│  │  App chat    │ ───────────────────► │  chat-bridge.mjs    │  │
│  │  composer    │                      │  127.0.0.1:4767     │  │
│  └──────────────┘                      │  spawn: exo         │  │
│         ▲                              │  conversation send  │  │
│         │ poll conversation_get_events └─────────────────────┘  │
│  useConversationEvents (cursor polling)                         │
└─────────────────────────────────────────────────────────────────┘
```

### Substrate reads (`ExoClient`)

All inspector data goes through a single typed client (`src/api/exoClient.ts`) that POSTs to `/request` with `{ kind: "request", id, request }` envelopes. The client only issues **read-only** request types (see `ReadOnlyRequest` in `src/api/protocol.ts`).

Default base URL is `/exo` (Vite proxy). The field accepts a server root (`http://127.0.0.1:4766`) or a full `/request` URL; both normalize to the request endpoint. Health uses `GET /health` on the same host.

### Executor adapter (`server/chat-bridge.mjs`)

The substrate HTTP API does **not** run agent turns or create conversations. Those writes post to same-origin paths proxied to `chat-bridge.mjs`:

- `POST /chat` validates `{ agent, conversation, message }`, spawns `exo --harness <harness> conversation send <agent> <conversation> <message>` (default harness: `exoclaw`), then optionally runs `exo conversation show` to read `latest_event_id` from stdout.
- `POST /chat/create` validates `{ agent, name? }`, spawns `exo --harness <harness> conversation create <agent> [name]`, and returns the new conversation's `slug` and `id` parsed from stdout.

Every spawn uses an argument array with `shell: false`. No streaming, no substrate reads beyond the CLI — it is an explicit, replaceable executor seam. A future native executor HTTP API can swap in without changing the UI contract.

### Live updates (cursor polling)

`useConversationEvents` repeatedly calls `conversation_get_events` with an ascending cursor after the last seen event id. Cadence:

- **1 s** while a turn is pending (chat or external).
- **2 s** after new events arrive.
- **Backoff to 15 s** when idle (×1.6 per empty poll).

After a chat turn completes, the app triggers one immediate poll. A future substrate `watch_events` endpoint would plug into this hook; the transcript renderer stays unchanged.

### Artifact viewer

Tool results and `artifact_written` events can reference artifacts. The viewer loads via `conversation_read_artifact` (read-only), caches by `(agent, conversation, artifact_id, version)`, and renders by content type (text, JSON, markdown, image).

## Features

| Area          | Behavior                                                                                                               |
| ------------- | ---------------------------------------------------------------------------------------------------------------------- |
| Navigation    | Agent list, conversation list, text filter                                                                             |
| Transcript    | User/assistant messages (markdown), reasoning disclosures, tool call cards with paired results, optional system events |
| Live chat     | Optimistic user bubble, typing indicator, Enter to send / Shift+Enter newline                                          |
| Export        | JSON and markdown download for the loaded event set                                                                    |
| Secrets       | Name, type, id, created_at, scope (global / agent / conversation) — **never values**                                   |
| Bindings      | env, mcp, llm, sandbox — metadata and config; secret ids shown as short ids, not fetched                               |
| Sandbox       | Derived from events: create/start/stop, processes, stdout/stderr counts                                                |
| Session stats | Rollup chips (tokens, cost, etc.) in header and state panel                                                            |
| Health        | Badge for idle / checking / ok / error against `/health`                                                               |
| Refresh       | Manual reload of agents, conversations, secrets, bindings                                                              |

## Theming

- **Light is default.** Dark mode is opt-in via the top-bar toggle; preference is stored in `localStorage` (`exo-theme`).
- Dark mode sets `data-theme="dark"` on `<html>`; CSS custom properties in `src/styles.css` switch the palette.
- Syntax highlighting uses GitHub-light / GitHub-dark token colors per theme.

## Accessibility

- Landmark regions: `nav` (agents/conversations), `main` (transcript), `aside` (state).
- Live regions: timeline (`aria-live="polite"`), chat status, screen-reader announcement when a reply completes.
- Visible labels and `.sr-only` text for filter, base URL, chat input, and icon-only controls.
- `focus-visible` rings on interactive elements; skeleton loaders marked `aria-hidden`.
- `prefers-reduced-motion: reduce` disables non-essential animation (typing indicator, transitions).

## Run locally

Three processes in separate terminals:

### 1. Exo substrate (port 4766)

```bash
exo serve --bind 127.0.0.1:4766
```

### 2. Chat bridge (port 4767) — required for sending messages

```bash
cd web
EXO_BIN=<path-to-exo> EXO_CWD=<exo-working-dir> npm run bridge
```

Environment variables:

| Variable               | Default         | Purpose                         |
| ---------------------- | --------------- | ------------------------------- |
| `EXO_BIN`              | `exo`           | CLI binary                      |
| `EXO_CWD`              | `process.cwd()` | Working directory for exo state |
| `EXO_HARNESS`          | `exoclaw`       | `--harness` flag value          |
| `CHAT_BRIDGE_PORT`     | `4767`          | Listen port                     |
| `CHAT_TURN_TIMEOUT_MS` | `300000`        | Kill child after timeout        |

Bridge health: `GET http://127.0.0.1:4767/health` → `ok`.

### 3. Vite dev server

```bash
cd web
npm install
npm run dev
```

Open the printed URL. Leave the **base** field as `/exo` so reads go through the Vite proxy (avoids CORS during development).

Proxy targets (override with env vars when starting Vite):

| Path    | Default target          | Rewrite                               |
| ------- | ----------------------- | ------------------------------------- |
| `/exo`  | `http://127.0.0.1:4766` | strips `/exo` prefix → substrate root |
| `/chat` | `http://127.0.0.1:4767` | no rewrite                            |

```bash
VITE_EXO_PROXY_TARGET=http://127.0.0.1:4766 \
VITE_CHAT_BRIDGE_TARGET=http://127.0.0.1:4767 \
npm run dev
```

### Production build

```bash
npm run build    # tsc -b && vite build
npm run preview  # static preview; configure reverse proxy for /exo and /chat
```

## Substrate API surface used

**Health:** `GET /health`

**Via `POST /request` (read-only):**

- `list_agents`
- `list_conversations` (paginated)
- `conversation_get_events` (paginated, asc cursor)
- `conversation_read_artifact`
- `list_secrets`, `agent_list_secrets`, `conversation_list_secrets`
- `list_bindings`, `agent_list_bindings`, `conversation_list_bindings`

**Not called:** `get_secret`, `put_secret`, `conversation_begin_turn`, `conversation_add_events`, or any other mutating request. Sandbox process I/O and mutation helpers are also unused.

**Write path (chat only):** `POST /chat` on the bridge → `exo conversation send`.

## Chat caveats

- The bridge runs a full CLI turn; expect seconds to minutes, not token streaming.
- If another adapter is writing to the same conversation, events can interleave. Prefer a dedicated conversation for web chat.
- Polling shows new events after they are committed to the substrate; there is no SSE chunk preview during generation.

## Project layout

```
web/
  server/chat-bridge.mjs   # local executor adapter
  src/
    api/                   # ExoClient + protocol types
    components/            # Transcript, SidePanels, ArtifactViewer, …
    lib/                   # polling hook, sandbox derivation, export
  vite.config.ts           # /exo and /chat proxies
```

See [`docs/DESIGN.md`](docs/DESIGN.md) for architectural decisions and trade-offs.
