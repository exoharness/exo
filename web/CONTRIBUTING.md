# Contributing to exo web

Local inspector UI for an exo substrate. Read [`README.md`](README.md) for a feature overview and [`docs/DESIGN.md`](docs/DESIGN.md) for architectural decisions. API details: [`docs/API.md`](docs/API.md).

---

## Prerequisites

- Node.js (project uses Vite 7, React 19, TypeScript 5.9)
- A running exo substrate (`exo serve`)
- The `exo` CLI on PATH (or `EXO_BIN`) for chat
- Three terminals for full local dev (substrate, bridge, Vite)

---

## Run locally

### 1. Substrate — port 4766

```bash
exo serve --bind 127.0.0.1:4766
```

### 2. Chat bridge — port 4767

Required only for sending messages from the composer.

```bash
cd web
EXO_BIN=<path-to-exo> EXO_CWD=<exo-working-dir> npm run bridge
```

| Variable               | Default         | Purpose                         |
| ---------------------- | --------------- | ------------------------------- |
| `EXO_BIN`              | `exo`           | CLI binary                      |
| `EXO_CWD`              | `process.cwd()` | Working directory for exo state |
| `EXO_HARNESS`          | `exoclaw`       | `--harness` value               |
| `CHAT_BRIDGE_PORT`     | `4767`          | Listen port                     |
| `CHAT_TURN_TIMEOUT_MS` | `300000`        | Kill child on timeout           |

Health: `curl http://127.0.0.1:4767/health` → `ok`.

### 3. Vite dev server

```bash
cd web
npm install
npm run dev
```

Open the printed URL. Keep the **base** field as `/exo` so reads use the Vite proxy.

Override proxy targets when starting Vite:

```bash
VITE_EXO_PROXY_TARGET=http://127.0.0.1:4766 \
VITE_CHAT_BRIDGE_TARGET=http://127.0.0.1:4767 \
npm run dev
```

| Proxy path | Target    | Rewrite              |
| ---------- | --------- | -------------------- |
| `/exo`     | substrate | strips `/exo` prefix |
| `/chat`    | bridge    | none                 |

---

## Commands

| Script      | Command                       | Purpose                                                             |
| ----------- | ----------------------------- | ------------------------------------------------------------------- |
| `dev`       | `vite`                        | Dev server with proxies                                             |
| `bridge`    | `node server/chat-bridge.mjs` | Chat executor adapter                                               |
| `build`     | `tsc -b && vite build`        | Typecheck + production bundle                                       |
| `typecheck` | `tsc --noEmit`                | Types only                                                          |
| `test`      | `vitest run`                  | Unit tests (`src/**/*.test.ts`, Node env)                           |
| `preview`   | `vite preview`                | Static preview; configure `/exo` and `/chat` reverse proxy yourself |

---

## Architecture seams

Keep new features on these boundaries so executor and transport can evolve independently.

```
Browser
  ExoClient ──POST /exo/request──► substrate (read-only)
  App chat  ──POST /chat────────► chat-bridge.mjs ──spawn──► exo conversation send
  useConversationEvents ──getEventsPage──► substrate (cursor polling)
  ArtifactContext ──readConversationArtifact──► substrate
  deriveSandboxState(events) ──pure──► side panel (no sandbox API calls)
```

### `ExoClient` → substrate

- Single typed entry for all inspector reads (`src/api/exoClient.ts`, types in `src/api/protocol.ts`).
- Only `ReadOnlyRequest` variants; no secret values, no turn/sandbox mutations.
- New read endpoints: add the request/response types to `protocol.ts`, a method on `ExoClient`, then wire UI state in `App.tsx` or a panel component.

### Chat bridge as executor adapter

- `server/chat-bridge.mjs` is intentionally thin: validate body, spawn `exo conversation send`, optionally parse `latest_event_id` from `exo conversation show`.
- UI contract is fixed: `POST /chat` with `{ agent, conversation, message }` → `{ ok: true }` on success.
- Replace the bridge with a native executor HTTP API without changing React if you preserve that contract.

### Cursor-polling seam

- `src/lib/useConversationEvents.ts` owns live updates.
- Calls `ExoClient.getEventsPage` with an ascending cursor, dedupes by event id, adjusts interval:
  - **1 s** while `turnPending`
  - **2 s** after new events
  - **Backoff to 15 s** on empty polls (×1.6 per streak)
- Exposes `poll()` for immediate refresh (used after chat completes).
- A future `watch_events` substrate endpoint should plug in here; transcript components stay unchanged.

### Artifact viewer

- `ArtifactContext` injects a loader from `Transcript` (wraps `readConversationArtifact`).
- `ArtifactViewer` lazy-loads on expand; classifies by path/extension → image, JSON, markdown, or plain text.
- `findArtifactRef` walks tool result JSON for nested `artifact_id` pointers.

---

## Project layout

```
web/
  server/chat-bridge.mjs     # executor adapter
  src/
    api/                     # ExoClient, protocol types
    components/              # Transcript, SidePanels, Markdown, …
    lib/                     # polling, sandbox fold, export, stats, copy
  docs/                      # DESIGN.md, API.md
  vite.config.ts             # dev proxies
```

---

## Code conventions (observed in repo)

- **TypeScript strict:** protocol shapes live in `protocol.ts`; avoid parsing loose `JsonValue` in UI when a struct exists.
- **No global store:** React `useState` / `useMemo` / `useEffect` in `App.tsx`; hooks in `src/lib/` for reusable logic.
- **Read-only inspector:** substrate writes only via `/chat` bridge; never add mutating `ExoRequest` calls without an explicit product decision.
- **Serde-style unions:** `EventData`, `ExoRequest`, `ExoResponse` use `type` discriminants; match existing patterns when extending.
- **Pure derivations:** sandbox state and conversation rollups are pure functions over `Event[]` — keep side effects in effects/handlers.
- **CSS:** global `styles.css` with custom properties; dark mode via `[data-theme="dark"]` on `<html>`.
- **Tests:** Vitest for lib and URL normalization; colocate as `*.test.ts` next to source. No component test harness yet.
- **Comments:** sparse; explain non-obvious seams (polling, artifact cache, scope dedupe) not obvious code.
- **Accessibility:** landmarks (`nav`, `main`, `aside`), `aria-live="polite"` for transcript/chat status, `.sr-only` labels, `focus-visible` rings.

---

## How to add a feature

Example: show conversation metadata from a new substrate read.

### 1. Protocol + client

1. Add request/response variants to `ExoRequest` / `ExoResponse` in `src/api/protocol.ts` if missing.
2. If read-only, include the request in `ReadOnlyRequest`.
3. Add a method on `ExoClient` that calls `this.send(request, expectedResponseType)` and returns the typed payload.
4. Add a unit test if the method has pagination, caching, or normalization logic.

### 2. Load data in `App.tsx`

1. Add state for the new data and loading/error flags.
2. Fetch in the appropriate `useEffect` (mirror agents → conversations → conversation-details layering).
3. Include `refreshToken` in effect deps so **Refresh** reloads it.
4. Pass props into `SidePanels`, `Transcript`, or a new component.

### 3. Render

1. Prefer extending existing components (`SidePanels`, `Transcript`) over new top-level shells.
2. Use `JsonPreview`, `MarkdownContent`, `CopyButton`, `SkeletonRows` for consistent inspector UX.
3. For event-derived data, consider a pure function in `src/lib/` with tests.

### 4. Live updates

- If the feature depends on new events, `useConversationEvents` already provides `events` — derive with `useMemo`.
- If you need faster refresh during turns, use `turnPending` and `poll()` rather than a second poll loop.

### 5. Writes

- Do **not** add substrate mutation from the browser.
- For actions that run agent turns, extend or replace the bridge while keeping `POST /chat` shape stable, or add a new same-origin path in `vite.config.ts` + a thin server adapter.

### 6. Verify

```bash
npm run typecheck
npm run test
npm run build
```

Manual: substrate + bridge + dev server; exercise the feature against a real conversation.

---

## Production notes

`npm run build` emits static assets. Serve them behind a reverse proxy that forwards `/exo` to the substrate and `/chat` to the executor adapter. The app does not embed a production server.
