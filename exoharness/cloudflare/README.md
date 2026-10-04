# Exo on Cloudflare: managed agents prototype

Exo's API, basic agent loop, state and credential proxy run in Workers and SQLite Durable Objects. Only tool execution uses a Linux sandbox. There is no Exo server image, proxy image, Dockerfile, or deployed Rust binary. Cloudflare's Linux Sandboxes are backed by Containers; the `containers` configuration attaches that execution capability to `ExoSandbox`.

```mermaid
flowchart LR
  Client[Exo managed agents client] --> Worker[Authenticated Worker]
  Worker --> Provider[ExoProvider Durable Object]
  Provider --> SQLite[(SQLite state and encrypted vault)]
  Provider --> R2[(R2 artifacts)]
  Provider --> Model[Model Responses API]
  Provider --> Sandbox[Per-thread Linux Sandbox]
  Sandbox --> Egress[ExoEgress Worker]
  Egress --> Provider
  Provider --> Upstream[Allowed HTTP/S upstream]
```

## Implemented

- A TypeScript `ExoHarness` implementation: agents, conversations, sessions, turns, canonical event history, artifacts and encrypted static-key vaults.
- The existing managed agents HTTP protocol under `/exo`: agent definitions as artifacts, threads, turns, status, cancellation, approvals, event queries and resumable SSE.
- The existing portable Responses runtime and Lingua conversion, including a precompiled WASM module. The agent loop runs in the Worker; `shell` runs through native sandbox `exec`.
- Per-thread sandbox execution using Cloudflare's managed `cloudflare/debian-trixie` image. No custom image build is needed for the test.
- Default-deny egress. HTTP port 80 and HTTPS port 443 go through a Worker binding whose trusted props identify the agent/thread. Commands cannot select a different identity through request headers.
- Opaque environment placeholders, Bearer/custom header and Basic auth substitution, destination checks, manual redirects and live vault revocation. Vault values stay outside the sandbox.
- Explicit filesystem snapshots and restoration after sandbox destruction. Artifacts are stored separately in R2.

The deployment is a single operator/account prototype, authenticated with `EXO_TOKEN`. Model credentials are resolved from attached vaults or the `global` vault and checked against their destination policy. Vault values use AES-GCM with the Worker secret `VAULT_KEY`; metadata is authenticated as associated data.

## Run and deploy

Install the repository dependencies and this package's dependencies:

```sh
pnpm install
pnpm --dir exoharness/cloudflare install
pnpm --dir exoharness/cloudflare typecheck
pnpm --dir exoharness/cloudflare test
```

`test` builds the real Worker bundle and runs Miniflare with real SQLite Durable Object and R2 storage. Its model and sandbox execution bindings are fixtures. Live tests below exercise the actual Cloudflare Linux sandbox.

`wrangler.jsonc` targets the separate `exo-managed-agents-spike` Worker and R2 bucket in Braintrust Enterprise. Change the account, Worker, bucket and `ACCOUNT_ID` for another deployment. After `wrangler login`, create the R2 bucket and deploy:

```sh
pnpm --dir exoharness/cloudflare exec wrangler r2 bucket create exo-managed-agents-spike
pnpm --dir exoharness/cloudflare run deploy
```

Set `EXO_TOKEN` to a random bearer token and `VAULT_KEY` to a random 64-character hex key with `wrangler secret put` or `wrangler secret bulk`. Keep `VAULT_KEY` stable for this deployment's encrypted state. Do not commit credentials. The optional `PROBE_KEY` enables a synthetic test receiver at `/_probe` that returns authentication booleans without reflecting the key.

The tested endpoint is `https://exo-managed-agents-spike.braintrust.workers.dev/exo`. The existing CLI can configure it with:

```sh
exo provider configure cloudflare-spike \
  --url https://exo-managed-agents-spike.braintrust.workers.dev/exo \
  --api-key-env EXO_CLOUDFLARE_TOKEN
```

Set `EXO_CLOUDFLARE_TOKEN` locally to this Worker's `EXO_TOKEN` before using the profile. This prototype has been tested through its HTTP protocol; the local Rust CLI binary was not built for this investigation.

## Live verification

The live test reads `OPENAI_API_KEY` from the repository `.env` and uploads it into the Worker's encrypted `global` vault. It reads the private `.local/secrets.json` used to provision `EXO_TOKEN`, `VAULT_KEY` and the synthetic `PROBE_KEY`. It creates a test agent/thread, runs the assertions, and stops its sandbox in a `finally` block. It leaves the agent history, snapshot and artifacts for inspection.

```sh
node exoharness/cloudflare/scripts/live-test.mjs
```

On October 4, 2026, live checks passed for Linux execution, opaque placeholders, HTTPS and Basic credential injection, denied destinations, unknown placeholders, bare-IP TLS with certificate validation disabled, direct UDP TXT DNS, snapshot restoration with interception reinstalled, R2 artifacts, a real model → shell → model turn, and revocation on a running sandbox. The ignored `.local/live-results.json` records test IDs and the passing assertions.

A TCP connection on port 443 can connect to the interceptor before TLS routing is checked. A successful socket connection alone is not evidence of external access. Intercepted A/AAAA DNS queries receive platform-generated addresses; the DNS bypass test uses a TXT query to an explicit public resolver.

## Limits and remaining work

This establishes that the platform supplies the essential gadgets; it is not a drop-in deployment of `exo serve`.

- Only the `basic` harness, an OpenAI-compatible Responses model, `shell`, and static key credentials are supported. OAuth refresh, GitHub CLI credentials, Codex/Claude/Pi subprocess harnesses, MCP, custom tool modules, resources, environment definitions, adapters, frontend tools and delivery callbacks require additional integration. Unsupported definition/request options fail explicitly.
- The account Durable Object centralizes metadata and runs multiple thread jobs. Production needs tenant authorization and a deliberate partitioning scheme, quotas, audit logging and encrypted-key rotation.
- A persisted alarm recovers unfinished jobs; model calls can repeat after a crash. Tool dispatch is journaled first. If a tool's outcome is ambiguous after interruption, the sandbox is stopped and the turn ends with an error; the command is never automatically replayed. This is not exactly-once execution.
- SSE sends canonical persisted events; token deltas are not streamed. Native `exec` exposes stdin/stdout/stderr, PTY and exit/signals, but this prototype collects shell output. Long-lived process handles and preview/port routing are not exposed through the Exo API yet.
- Egress rules allow exact public HTTP/S origins on ports 80/443. Extra ports need native intercept registrations. Generic TCP/UDP proxying is outside this implementation.
- Sandboxes stop after five minutes of inactivity. Workspace preservation requires an explicit snapshot before stop. Snapshots preserve files, not running processes; Cloudflare currently expires unused snapshots after 30 days and ties them to their source image. Production should add checkpoint policy and R2 directory backups for longer retention/image migration.
- The built-in image has Node 24 and a minimal Linux userspace. Other agents need their dependencies installed or a suitable execution image. Node, Python, curl and Git trust variables point to Cloudflare's interception CA; only Node's HTTPS behavior was exercised live here.
- Agent/thread deletion, artifact operations and forks need further concurrency/cleanup work before treating this as production storage. API coverage is intentionally partial; unimplemented routes return 404.

Native Sandbox references: [run Linux commands](https://developers.cloudflare.com/sandbox/get-started/), [container API and interception](https://developers.cloudflare.com/containers/api/durable-object-container/), [authenticated API calls](https://developers.cloudflare.com/sandbox/network/call-an-authenticated-api/).
