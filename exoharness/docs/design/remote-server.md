# Remote Exo server

Status: proposed. The `/exo` namespace cleanup is implemented; the authentication, sharing, and UI work below is not.

## Product behavior

One Exo server supports personal access from multiple devices and shared access for a team. Admission and sharing are separate settings.

| Deployment                | Who can log in                 | Agents and threads                                   | Vaults                                                     |
| ------------------------- | ------------------------------ | ---------------------------------------------------- | ---------------------------------------------------------- |
| Personal                  | The configured owner           | Private to the owner, available on all their devices | The owner's personal vault                                 |
| Team                      | Explicitly admitted identities | Private to their creators by default                 | One personal vault per identity                            |
| Team with `--multiplayer` | The same admitted identities   | Shared among admitted users                          | Personal vaults remain private; shared vaults are explicit |

Multiplayer shares conversation history, artifacts, and thread files. It does not grant access to another user's credentials. This is a server for trusted members with private data by default, not a hostile multi-tenant hosting service.

## CLI and HTTP surface

Make `exo serve` the canonical command for serving the complete local provider. Move the current `exo agent serve` implementation into it; retain agent filtering as `--agent NAME` on the same service. Keep adapter supervision in that process, including its existing adapters-only mode.

Proposed commands, not yet implemented:

```sh
# Local use; bind to loopback by default.
exo serve

# Authenticated access from other devices or teammates.
exo serve --auth-file ./server-auth.yaml --bind 0.0.0.0:4766
exo serve --auth-file ./server-auth.yaml --bind 0.0.0.0:4766 --multiplayer

# On any client device.
exo provider create personal --url https://exo.example.com/exo
exo provider login personal
exo provider switch personal
exo agent run --agent developer
```

`--auth-file` supplies typed issuer/client configuration, the operator identity, and admitted identities. An operator-owned vault reference supplies an OAuth client secret if needed; that vault is never automatically attached to user runs. Parse every option and environment-variable binding through Clap. Do not reintroduce a server `--token-env` option or implicit environment reads.

Serve the managed API under `/exo`, the web UI under `/`, and auth handlers under `/exo/auth`, plus the standard OAuth discovery locations required by the login protocol. Do not expose the lower-level ExoHarness `/request` transport on this listener.

Complete the same HTTP contract used by `RuntimeClient`: identity; agents and definitions; threads, history, artifacts, environments, turns, cancellation and approvals; vault and secret management. Vault reads expose metadata, never resolved credential values. Add missing remote operations using the shared request types and vault interfaces, rather than a UI-specific API.

## Authentication

Start with OIDC for both personal and team deployments. Exo handles the browser login callback and creates an Exo session; the browser uses a secure HttpOnly cookie and the CLI uses a bearer credential. CLI and browser must resolve to the same principal. Validate the issuer, audience, signature, expiry, login state, and nonce using a maintained implementation; protect cookie-authenticated mutations against CSRF.

The first implementation checkpoint is a working login through the existing `exo provider login` client. That client already implements OAuth metadata discovery, PKCE, credential storage, refresh, and account pinning through RMCP. OIDC login alone does not provide its full server contract: prove discovery, client registration or configured client IDs, the loopback callback, token exchange, refresh, and logout before committing to a server-side library. Any required client-registration extension belongs in the existing provider auth code.

Authentication is required for remote access. Keep unauthenticated local serving limited to loopback and the local owner; a reverse proxy must not publish that mode. Terminate HTTPS at a configured reverse proxy initially. Do not trust arbitrary forwarded identity headers. Admission is configured by the operator, with no unauthenticated first-visitor claim endpoint.

Use a stable principal key derived from verified identity-provider data: `(issuer, subject)` for OIDC. Names and email addresses are display fields, not ownership keys. Return the stable Exo principal ID from `/exo/identity` so existing provider/account pinning continues to work.

Persist server sessions and refresh state outside agent-accessible storage. Reuse existing encryption/key-provider facilities for sensitive server material. Keep inbound Exo sessions distinct from outbound MCP credentials, while reusing the storage primitives. Logging out revokes that session; removing an admitted identity invalidates its sessions and closes authenticated subscriptions. Other devices retain their sessions after a device-local logout.

Built-in accounts/invites, identity linking across different issuers, and trusted-proxy authentication can follow if needed. They must produce the same principal and vault context.

## Identity-to-vault association

Persist a small record in the server's existing state:

```text
(issuer, subject) -> principal_id, personal_vault_id
```

- After authentication and admission succeed, atomically look up or create the principal and its personal vault. Concurrent first logins must not create duplicate vaults.
- Returning users and additional devices reuse the same association. Display the default vault as `personal`; its stored ID is the authority, not its display name.
- Let the local operator explicitly link an existing vault to an admitted identity. Do not infer ownership from a matching name or email.
- New personal vaults start empty. Users add model credentials or connect MCP services once, using the existing vault/OAuth flow.
- Additional vaults created by a user belong to that principal. Optional server-configured shared vaults grant use to admitted users; only the operator manages their credentials.

Authentication produces a caller-scoped `VaultContext` containing the personal vault and explicitly granted shared vaults. Attachments and persisted secret references must be checked against that context on every use. A client-supplied vault ID, a shared agent, or a saved thread is not an authorization grant.

Keep `VaultHandle`, secret IDs, encryption, OAuth refresh, destination checks, rotation, revocation, and credential substitution as the implementation. Custom providers and external vault backends can supply their own principal-to-context mapping.

Existing local state needs an explicit owner assignment when enabling auth. Preserve vault/agent/thread IDs and account aliases; make the assignment restart-safe. Do not expose an existing local `global` vault to newly admitted users by default.

## Private and multiplayer execution

Store creator identity on agents and threads. A small server policy checks admission, ownership, and the single `--multiplayer` setting; OSS does not need projects, organizations, or arbitrary ACLs. Server configuration, admission, shared environments, and shared-vault administration remain operator-controlled.

In private mode, filter lists and enforce ownership on direct lookups, artifacts, event streams, forks, mutation, cancellation, and approvals. Apply checks in the shared runtime/provider boundary so adapters and background execution cannot bypass them.

In multiplayer mode, admitted users can discover and work with shared agents and threads. Every accepted turn records the invoking principal and its authorized vault selection. Only that principal can approve use of its credentials; other viewers cannot approve on its behalf. Cancellation may be shared without granting approval authority.

Do not reuse a credential-bearing warm harness, sandbox proxy binding, or MCP client across principals. When a different user continues a shared thread, invalidate the previous execution context and establish one for the new caller before running. The thread's shared history/files remain visible, but old vault references cannot become ambient credentials. Keep one active turn per thread.

Durable jobs and adapters run under an explicitly recorded principal and vault grant, independent of a browser session's lifetime. Recheck admission and vault access when dispatching. Revocation rejects new work and invalidates affected cached connections; already-sent external requests may finish.

## Minimal web UI

Serve a small static React application from the same process and origin. Start with an agent list, thread list, transcript/composer, streaming tool activity, an approval dialog, cancel/reconnect, and a personal-vault connections screen. Show the active identity and when history is shared.

Use assistant-ui's existing thread/composer components with an external-store adapter backed by Exo history and SSE. Exo owns persistence and execution; the UI does not call models or keep a second conversation database. Add only the small Exo-specific pieces for tool approvals and vault connections. Reconnect from the existing event cursor and deduplicate events.

## Implementation sequence and acceptance

1. **Namespace and serving.** `/exo` is the only OSS managed API namespace; move the command to `exo serve` and keep one service implementation. Verify the existing HTTP client against full-provider and agent-filtered servers. The raw `/request` route stays absent.
2. **Login and personal vaults.** Prove the complete CLI/browser login flow, session refresh/logout, stable identity, atomic vault provisioning, and explicit adoption of existing local state. Verify that two devices get the same vault, two identities get different vaults, and rejected identities create no state.
3. **Authorized provider operations.** Finish remote vault CRUD and private-mode enforcement using caller-scoped handles. Exercise agents, threads, artifacts, history/SSE, cancellation, approvals, environments, and vault operations as two users, including direct-ID requests and revoked access. Keep this in one enforcement path shared with non-HTTP execution.
4. **Multiplayer.** Add the server flag and exercise two users continuing the same thread. Verify shared history, separate credential selection, caller-only approvals, revoked grants, and no credential/session reuse across principals. Test restart, background jobs, and switching sharing mode without losing creator metadata.
5. **Web UI and end-to-end demo.** Reuse the same API for browser and CLI. Sign in from a second device, reconnect to a saved turn, approve/cancel, connect Notion in a personal vault, rotate/revoke it, and repeat with a second user in both modes.

Ship these as separate reviewable changes. Do not mark remote serving complete until auth, ownership, and vault enforcement are all present; a route rename or UI login screen alone is insufficient.

## References

- [Omnigent auth and SSO](https://omnigent.ai/docs/collaborate/auth): inspiration for personal/team login and operator-controlled admission; its broader account system is not required for the first Exo implementation.
- [OIDC stable identity](https://openid.net/specs/openid-connect-core-1_0.html#ClaimStability): use issuer and subject for identity.
- [assistant-ui ExternalStoreRuntime](https://www.assistant-ui.com/docs/runtimes/custom/external-store): connect existing components to Exo-owned state and callbacks.
- Existing code: `crates/cli/src/serve.rs`, `crates/cli/src/providers/oauth.rs`, `crates/executor/src/http_service.rs`, and `crates/exoharness/src/vault.rs`.
