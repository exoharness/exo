# exo web — design decisions

This document explains **why** exo web is shaped the way it is. It is written against the current code, not an aspirational product spec.

## Goals

1. **Inspect** what the exo substrate already recorded — agents, conversations, events, bindings, secrets metadata, sandbox activity.
2. **Chat** with an agent from the browser without pretending the substrate is a chat server.
3. Stay **small** — no admin console, no multi-user auth, no deployment story baked in.

## Non-goals

- Replacing the `exo` CLI or a full IDE.
- Managing secrets, bindings, agents, or sandboxes from the UI (no create/update/delete).
- Token-level streaming UX (SSE/WebSocket) until the substrate exposes a real watch API.
- Production hardening (auth, rate limits, audit logging) — this is a local dev/ops inspector.

---

## Substrate vs executor boundary

**Decision:** All substrate access is read-only through `ExoClient`. Agent turns go through a separate local **executor adapter** (`server/chat-bridge.mjs`).

**Why:**

- The exoharness HTTP API is a **substrate transport**: typed requests, durable events, artifacts. It is not an executor. There is no `begin_turn` call from the web app, and adding a fake “streaming chat” route on the substrate would blur that boundary.
- Running a turn requires harness context, CLI credentials, and process lifecycle that already live in `exo conversation send`. The bridge shells that command instead of reimplementing turn logic in Node or the browser.
- The seam is explicit and swappable: `POST /chat` with a JSON body is the UI contract. A future `exo serve`-level executor endpoint can replace `chat-bridge.mjs` without touching React components.

**Trade-off:** Chat depends on a second process and a correctly configured `EXO_BIN` / `EXO_CWD`. Failures surface as bridge 500s with stderr previews, not as substrate errors.

**Trade-off:** Turns are **blocking** from the UI’s perspective (wait for CLI exit, then poll). That matches how the CLI works today; it is honest, not fast.

---

## Why not fake streaming on the substrate?

A browser chat UI often expects SSE or WebSockets. exo web deliberately does **not** simulate that by:

- Polling `conversation_get_events` and rendering committed events.
- Showing an optimistic user message and a typing indicator while the bridge runs.

**Why:** Partial events on the substrate may not match what a stream-oriented UI needs (ordering, deduping, tool interleaving). Polling committed events is simpler and matches the inspector’s “read what was recorded” model.

**Future path:** `useConversationEvents` is the single seam. A `watch_events` (or similar) substrate endpoint would replace the timer loop inside that hook; `getEventsPage` stays the canonical page fetch. Transcript, tool cards, and artifact viewer do not need to change.

Polling parameters today:

| State              | Interval              |
| ------------------ | --------------------- |
| Turn pending       | 1 s                   |
| New events         | 2 s                   |
| Idle (empty polls) | 2 s × 1.6^n, cap 15 s |

---

## Secrets: names and metadata only

**Decision:** The UI lists `SecretMetadata` (id, name, type, created_at) at global, agent, and conversation scope. It never calls `get_secret` or displays values.

**Why:**

- Secret values in a browser are a footgun — DevTools, extensions, screen shares, accidental copy.
- The inspector’s job is to answer “what is wired?” not “what is the key?”. Bindings reference `secret_id` as short ids for the same reason.
- Read-only substrate access is enforced in types: `ReadOnlyRequest` excludes `get_secret` / `put_secret`.

**Trade-off:** You cannot verify a secret’s value from exo web. Use the CLI or your secret store.

---

## No heavy chat framework

**Decision:** Custom transcript rendering on top of `react-markdown` (+ GFM, math, highlight, mermaid). No chat SDK, no virtualized message list library, no global state store.

**Why:**

- The data model is **events**, not chat messages. A turn emits `messages`, `tool_requested`, `tool_result`, `artifact_written`, sandbox events, etc. Off-the-shelf chat UIs assume a simpler message stream.
- Tool call pairing, orphan results, system event toggles, and raw JSON disclosures are domain-specific.
- Bundle size and upgrade surface stay small (React 19 + Vite + a few markdown deps).

**Trade-off:** More bespoke UI code. Acceptable for an inspector, not for a consumer chat product.

---

## Light-first theming

**Decision:** CSS custom properties; light palette is `:root` default; dark via `[data-theme="dark"]` and `localStorage`.

**Why:**

- Local dev inspectors are often used in bright environments; dark-as-default adds friction.
- One attribute flip recolors surfaces, hairlines, code tokens, and syntax themes — no JS theme object or CSS-in-JS runtime.

**Trade-off:** No system `prefers-color-scheme` auto-switch yet; user must toggle (preference persists).

---

## Read-only artifact viewer

**Decision:** Artifacts load lazily via `conversation_read_artifact`, cached per client instance, rendered by sniffed type (image / json / markdown / text).

**Why:**

- Event payloads often hold references (`artifact_id`, `path`, `version`), not inline bytes. Loading on “view” keeps initial transcript fetches light.
- Immutability per version makes client-side caching safe for the session.

**Trade-off:** Large artifacts load in one shot; no range requests or progressive download.

---

## Sandbox state from events

**Decision:** Sandbox panels are **derived** by folding conversation events (`deriveSandboxState`), not a live sandbox control API.

**Why:**

- The inspector already has the event log; sandbox create/start/process events are enough for a debug view.
- Calling sandbox mutation or follow APIs would violate read-only scope and suggest controls the UI does not implement.

**Trade-off:** State can lag slightly behind reality if events are still landing; refresh and polling eventually converge.

---

## Vite proxies for local dev

**Decision:** Default base URL `/exo`; chat posts to `/chat`. Vite proxies to 4766 and 4767.

**Why:**

- Browsers block cross-origin fetches to `127.0.0.1:4766` from the Vite origin without CORS headers.
- Same-origin paths keep `fetch` simple and mirror how a reverse proxy would deploy the static build.

**Trade-off:** Production `preview` needs manual proxy configuration; the app does not embed a server.

---

## Accessibility choices (pragmatic, not certified)

- **Polite** live regions for transcript growth and chat status — enough for turn completion without shouting every token.
- **Landmarks** and sr-only labels on compact chrome (filter, base URL, composer).
- **focus-visible** and reduced-motion respect common keyboard and vestibular needs.

Not attempted: full WCAG audit, high-contrast theme variant, or keyboard shortcuts beyond standard form behavior.

---

## Summary table

| Topic        | Choice                     | Main cost                     |
| ------------ | -------------------------- | ----------------------------- |
| Turns        | CLI bridge, not substrate  | Extra process, blocking turns |
| Live updates | Cursor polling             | Latency, no stream preview    |
| Secrets      | Metadata only              | No in-UI value check          |
| UI stack     | Custom event transcript    | Maintenance                   |
| Theme        | Light default, CSS vars    | No auto system theme          |
| Scope        | Read-only inspector + chat | No admin/mutation tools       |
