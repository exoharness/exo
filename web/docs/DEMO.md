# Demo mode (no live exo backend)

The `src/api/mockData.ts` and `src/api/mockClient.ts` modules provide a
type-correct offline dataset: two agents, four conversations, and a rich event
stream in **Protocol Demo** that exercises messages, `<think>`,
tool calls, artifacts, markdown (code, tables, math, mermaid), secrets metadata,
bindings, and sandbox activity.

## Enable demo mode

Make one edit in `src/App.tsx` — swap the real client for `MockClient`:

```typescript
import { MockClient } from "./api/mockClient";
// import { ExoClient, normalizeRequestEndpoint } from "./api/exoClient";

// inside the useMemo that builds clientState:
return { client: new MockClient(), error: null };
// return { client: new ExoClient(activeBaseUrl), error: null };
```

Then start the UI as usual:

```bash
npm run dev
```

Open **Research Assistant → Protocol Demo** for the full fixture transcript.
The base URL field and `/health` check are bypassed; the inspector loads from
in-memory fixtures.

## Revert

Restore the `ExoClient` import and constructor line. No other files need to change.

## Verify fixtures typecheck

```bash
npm run typecheck
```
