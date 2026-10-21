# Exo on Cloudflare

The shared Rust `Runtime`, `LocalProvider` and `BasicExoHarness` run as WebAssembly in Workers and SQLite Durable Objects. JavaScript supplies storage, HTTP transport and Linux sandbox processes. Cloudflare Sandboxes use Containers; Exo orchestration stays in Workers.

Each thread has its own Durable Object and `StoredTurnCoordinator`. Turn acceptance saves the queue and recovery alarm atomically before returning `202`. The account object owns agents, events, encrypted vaults and the shared sandbox manager. Objects over 64 KiB go to R2; smaller values stay in SQLite. Managed routes use the native Rust dispatcher. The authenticated `/exo/request` endpoint exposes account-admin store RPC; use managed turn routes for coordinated execution.

The deployment supports basic and Codex harnesses, OpenAI-compatible Responses models, Git URL resources and remote HTTP MCP servers. Agent definitions, vault selection, credential policies and tool permissions use shared Exo code.

## Build and deploy

Install repository dependencies, the Wasm target and matching binding generator. Build artifacts stay under the repository:

```sh
pnpm install --frozen-lockfile
rustup target add wasm32-unknown-unknown
cargo install wasm-bindgen-cli --version 0.2.115 --locked \
  --root exoharness/cloudflare/.local/tools --target-dir target/wasm-bindgen-cli
pnpm --dir exoharness/cloudflare test
pnpm --dir exoharness/cloudflare typecheck
```

Tests build the real Worker bundle and use Miniflare SQLite/R2 with model and sandbox fixtures. They cover the shared Rust HTTP trait contracts, turn coordination, auth, SSE, persistence, egress, MCP and Codex process reuse. Adapter tests cover container lifecycle. Actual Linux execution and Codex usage are checked by live tests below.

Choose the account, Worker name and R2 bucket in `wrangler.jsonc`. `ACCOUNT_ID` names the shared Exo account's Durable Object; it is separate from the Cloudflare account ID. For Codex, copy its image as described below before deploying.

```sh
pnpm --dir exoharness/cloudflare exec wrangler login
pnpm --dir exoharness/cloudflare exec wrangler r2 bucket create exo-managed-agents
pnpm --dir exoharness/cloudflare deploy
```

Use `wrangler secret put` or `wrangler secret bulk` to set `VAULT_KEY` to a random 64-character hex key. Keep it stable: vault values use AES-GCM with authenticated metadata. For static auth, also set `EXO_TOKEN` to a random bearer token. Do not commit credentials.

## Authentication and CLI

With static auth, configure the existing client `--api-key-env` flow:

```sh
exo provider create cloudflare \
  --url 'https://exo-managed-agents.<your-subdomain>.workers.dev/exo' \
  --api-key-env EXO_CLOUDFLARE_TOKEN
```

Set `EXO_CLOUDFLARE_TOKEN` locally to the Worker's `EXO_TOKEN`, then run `exo provider login cloudflare`.

For OAuth, put the Worker behind [Cloudflare Access](https://developers.cloudflare.com/workers/configuration/cloudflare-access/) with [Managed OAuth](https://developers.cloudflare.com/cloudflare-one/access-controls/applications/http-apps/managed-oauth/), and configure `ACCESS_AUD`. Register the provider without `--api-key-env`; `exo provider login cloudflare` uses Exo's existing discovery, PKCE and refresh flow. Access's verified context is checked at the front door before trusted Durable Object RPC. Static tokens cannot bypass Access. Every approved caller shares the account's agents and vaults; per-user authorization is not implemented.

Local Access simulation in `wrangler.jsonc` requires a matching `vars.ACCESS_AUD`:

```jsonc
"access": { "dev": { "aud": "exo-local", "identity": { "email": "operator@example.com" } } }
```

Create the model credential in an Exo vault, then run the shared example:

```sh
exo --provider cloudflare vault secret create global openai \
  --token-env OPENAI_API_KEY --allow-origin https://api.openai.com
exo --provider cloudflare agent run --agent-file exoharness/examples/managed-agents/coder.md
```

The example selects Codex and `gpt-6.1-sol`. Resume with `exo --provider cloudflare agent run --agent <slug> --thread <slug>`. The sandbox's `/workspace` is remote. Model credentials come from attached vaults or `global`; model origins are permitted automatically, and other origins require a sandbox policy.

## Codex image

Basic agents use `cloudflare/debian-trixie`. Codex uses `codex-<PIN_PREFIX>`, the first eight hex characters of the image pin in [`TypeScriptHarnessPreset::sandbox_image`](../../crates/executor/src/managed_agents/config.rs). A pin change requires the corresponding configured image. The shared startup command also checks `containers/codex-sandbox/version`; acquisition never installs Codex.

Cloudflare's `durable_object` scheduling policy requires custom images in its managed registry. Manually copy the existing pinned image from GHCR, with Docker running and Wrangler authenticated:

```sh
EXO_CODEX_SOURCE='ghcr.io/exoharness/codex-devbox@sha256:<PIN_FROM_CONFIG_RS>'
EXO_CODEX_NAME='codex-<FIRST_8_HEX_CHARACTERS_OF_PIN>'
docker buildx imagetools inspect --raw "$EXO_CODEX_SOURCE"
# Select the linux/amd64 manifest digest from that output:
EXO_CODEX_AMD64='ghcr.io/exoharness/codex-devbox@sha256:<AMD64_MANIFEST_DIGEST>'
docker pull --platform linux/amd64 "$EXO_CODEX_AMD64"
docker tag "$EXO_CODEX_AMD64" "exo-codex-devbox:$EXO_CODEX_NAME"
pnpm --dir exoharness/cloudflare exec wrangler containers push "exo-codex-devbox:$EXO_CODEX_NAME"
```

Copy the amd64 manifest rather than the multi-platform index. Wrangler prints the registry reference; get its digest with `docker image inspect --format '{{join .RepoDigests "\n"}}' <registry-reference>` and configure it:

```jsonc
{
  "class_name": "ExoSandbox",
  "scheduling_policy": "durable_object",
  "images": {
    "codex-<PIN_PREFIX>": {
      "image": "registry.cloudflare.com/<CLOUDFLARE_ACCOUNT_ID>/exo-codex-devbox@sha256:<DIGEST>",
    },
  },
}
```

Deploy after configuring the image. Repeat the manual copy when adopting a new Codex image; no registry credentials are granted to agents. See [image management](https://developers.cloudflare.com/containers/guides/image-management/#use-an-external-image).

Codex reuses its app-server and thread while warm. A backend activity guard keeps the sandbox available during turns. Idle expiry checkpoints files and stops processes; `/home/exo/.codex` preserves native history. Automatic idle shutdown preserves the logical sandbox. Explicit Exo `stop_sandbox` ends it; use snapshot/stop/start to restore it. Cancellation closes the managed process.

## Git, MCP and sizing

Use ordinary `resources` and `mcp_servers` fields and attach credentials with `--vault`. Private Git resources also need `credential: github`; with the remote provider selected, `exo vault secret create <vault> --preset github` copies the local GitHub CLI token into that vault. The Worker cannot use your laptop's credential helper. MCP uses shared discovery, filtering, permissions and vault OAuth refresh.

Git preparation performs a shallow, single-branch checkout; an explicit SHA is fetched once at depth one. Preparation happens before the first turn. Files and the preparation marker survive snapshots, so later turns retain agent edits without fetching again. A changed resource URL, checkout or mount requires a new thread. Git credentials are egress placeholders, never plaintext sandbox environment values. Use `git fetch --deepen=<count>` or `--unshallow` for history.

Environment `resources` selects the smallest [instance type](https://developers.cloudflare.com/containers/platform/limits/) meeting both `vcpu_count` and `memory_mib`, allowing overprovisioning. Requests above 4 vCPUs or 12288 MiB fail. The type supplies its standard disk and persists across restarts. Without environment resources, `SANDBOX_INSTANCE` supplies a named type or custom `{ "vcpu": 1, "memoryMib": 3072, "diskMb": 20000 }`; otherwise the default is `lite`. Large Git repositories need room for both the checkout and database.

## Live tests

Set `EXO_WORKER_URL` to the origin without `/exo` and `EXO_TOKEN` to its bearer token. Agent modes need the `openai` vault credential; Codex optionally accepts `EXO_CODEX_MODEL`. Shell variables or Node's `--env-file` may supply configuration.

```sh
node exoharness/cloudflare/scripts/live.mjs basic
node exoharness/cloudflare/scripts/live.mjs codex
node exoharness/cloudflare/scripts/live.mjs contracts
```

Agent tests stop sandboxes in `finally` and retain history, artifacts and snapshots for inspection. Codex tests warm reuse, usage and explicit snapshot restoration. Contracts run shared Rust core contracts against `/exo/request`.

## Limits

- Only the Cloudflare sandbox backend is registered. Other backends need wasm-compatible implementations. Custom images must be registered by name; images must supply their tools, including Git.
- Codex requires `always_allow`. Custom tool modules, adapters, frontend tools, delivery callbacks, local MCP transports, local directories, read-only resource mounts and basic-harness turn limits on Codex are unsupported. GitHub CLI credentials must be imported as keys.
- Turn queues are independent, but state I/O still shares the account object. Recovery alarms drain only their thread. Model calls can repeat after crashes; tools with ambiguous outcomes fail rather than being replayed. Execution is not exactly-once.
- SSE combines persisted events with ephemeral Codex chunks. Basic model/shell output is collected. Process RPC is supported; preview/port routing is not.
- Exec timeouts signal only the direct process; descendants can hold output open. Use image-provided process-group timeouts when needed. See [process termination](https://developers.cloudflare.com/containers/guides/execute-commands/#stop-the-processes-a-command-starts).
- Egress enforces disabled, limited-host or unrestricted networking plus credential destinations. HTTPS interception covers public DNS names on ports 80/443; other networking is disabled. Redirects are manual. A rejected credential is refreshed/retried once for bodyless requests; uploads stream once without replay buffering.
- Sandboxes checkpoint after five idle minutes or explicit stop. Snapshots preserve files, not processes; crashes can lose changes since the checkpoint. Snapshots retain their original image, so image changes need a fresh sandbox. Unused snapshots expire after 30 days; longer retention or image migration needs separate backups.
