# Remote Exo server

Run the complete Exo provider over HTTP with OIDC login, private vaults, and optional shared agents and threads. The web UI is deferred.

## Product behavior

One Exo server supports personal access from multiple devices and shared access for a team. Admission and sharing are separate settings.

| Deployment                | Who can log in                 | Agents and threads                                   | Vaults                                                     |
| ------------------------- | ------------------------------ | ---------------------------------------------------- | ---------------------------------------------------------- |
| Personal                  | The configured owner           | Private to the owner, available on all their devices | The owner's personal vault                                 |
| Team                      | Explicitly admitted identities | Private to their creators by default                 | One personal vault per identity                            |
| Team with `--multiplayer` | The same admitted identities   | Shared among admitted users                          | Personal vaults remain private; shared vaults are explicit |

Multiplayer shares conversation history, artifacts, and thread files. It does not grant access to another user's credentials. This is a server for trusted members with private data by default, not a hostile multi-tenant hosting service.

## CLI and HTTP surface

`exo serve` serves the complete local provider. `--agent NAME` filters the same service to one saved agent. Keep adapter supervision in that process, including its existing adapters-only mode. Serving uses the built-in local runtime at `--root` (default `.exo`); creating a named provider profile is optional.

The default bind address is `127.0.0.1:4766`. Without `--auth-file`, authentication is disabled and requests run as the local owner; reject non-loopback binds. Supplying `--auth-file` enables authentication without changing the bind address. A non-loopback listener requires both auth configuration and an explicit `--bind`.

Commands:

```sh
# Local use: 127.0.0.1:4766, no authentication.
exo serve

# Authenticated server behind an HTTPS reverse proxy; still binds loopback.
exo serve --auth-file ./server-auth.yaml
exo serve --auth-file ./server-auth.yaml --multiplayer

# On any client device.
exo provider create personal --url https://exo.example.com/exo
exo provider login personal
exo provider switch personal
exo agent run --agent developer
```

`--auth-file` supplies typed issuer/client configuration, the operator identity, and admitted identities. An operator-owned vault reference supplies an OAuth client secret if needed; that vault is never automatically attached to user runs. Parse every option and environment-variable binding through Clap. Do not reintroduce a server `--token-env` option or implicit environment reads.

Serve the managed API under `/exo` and auth handlers under `/exo/auth`, plus the standard OAuth discovery locations required by the login protocol. Reserve `/` for the deferred web UI. Do not expose the lower-level ExoHarness `/request` transport on this listener.

Complete the same HTTP contract used by `RuntimeClient`: identity; agents and definitions; threads, history, artifacts, environments, turns, cancellation and approvals; vault and secret management. Vault reads expose metadata, never resolved credential values. Add missing remote operations using the shared request types and vault interfaces, rather than a UI-specific API.

## Authentication

Run authentication inside the existing Rust `exo serve` process, using the same HTTP listener and local state. Connect directly to the configured Google/OIDC provider. The MVP setup is explicit: the operator registers a Google app and callback URL, stores its client secret in a vault, writes one auth config file, and starts Exo. No shared login service, additional auth deployment, or setup wizard.

`openidconnect` handles discovery, code exchange, and signed identity-token validation; `oxide-auth` supplies the S256 PKCE verifier. Exo implements the CLI-facing registration, authorization-code, token, and revocation endpoints, persisting sessions and ownership through the existing encrypted vault store. These handlers share the runtime HTTP listener.

The browser uses a secure HttpOnly Exo session cookie and the CLI uses an Exo bearer credential. Both resolve to the same principal. Validate the issuer, audience, signature, expiry, login state, and nonce through the protocol library; protect cookie-authenticated mutations against CSRF.

Authentication is required for remote access. Keep unauthenticated local serving limited to loopback and the local owner; a reverse proxy must not publish that mode. Terminate HTTPS at a configured reverse proxy initially. Do not trust arbitrary forwarded identity headers. Admission is configured by the operator, with no unauthenticated first-visitor claim endpoint.

Use a stable principal key derived from verified identity-provider data: `(issuer, subject)` for OIDC. Names are display fields. Verified email allowlists can select users for enrollment, but ownership is pinned to the stable identity rather than the email. Return the stable Exo principal ID from `/exo/identity` so existing provider/account pinning continues to work.

Persist server sessions and refresh state in Exo-owned local state outside agent-accessible storage. Persist sessions as hashes of opaque random tokens through existing encryption/key-provider facilities; operators should not have to mint a separate cookie secret. Keep inbound Exo sessions distinct from outbound MCP credentials, while reusing the storage primitives. Logging out revokes that session and closes its event subscriptions. Restart after changing admission or sharing configuration; removed identities cannot reuse their persisted sessions. Other devices retain their sessions after a device-local logout.

Built-in accounts/invites, identity linking across different issuers, and trusted-proxy authentication can follow if needed. They must produce the same principal and vault context.

### Provider login and vault OAuth

| Flow                                        | What it authorizes                                                              | Credential owner                                         |
| ------------------------------------------- | ------------------------------------------------------------------------------- | -------------------------------------------------------- |
| `exo provider login personal`               | A user accessing the Exo server and their permitted agents, threads, and vaults | The CLI's existing credential store holds an Exo session |
| OAuth while adding an MCP secret to a vault | An agent accessing a service such as Notion                                     | The selected vault holds the service credentials         |

Implement the standard OAuth contract consumed by the existing RMCP-based provider client. Omnigent's custom CLI login-ticket polling is not part of this design. The provider flow is:

1. An unauthenticated `/exo/identity` request advertises protected-resource metadata pointing to Exo's authorization-server metadata.
2. The CLI discovers the authorization, token, and registration endpoints. Support its dynamic registration of public clients with loopback callbacks, or an explicitly configured client ID. Client registration identifies the CLI application and grants no user access.
3. The CLI opens Exo's authorization endpoint with state and an S256 PKCE challenge. Exo starts its separate Google/OIDC login, with independently bound state, nonce, and PKCE, and receives Google's response at the configured `/exo/auth/callback`.
4. After validating identity and admission, Exo resolves the principal and personal vault, then redirects a short-lived, single-use Exo authorization code to the CLI's registered callback. Bind the code to the client, redirect URI, resource, and PKCE challenge; the CLI exchanges it at Exo for Exo access and refresh tokens.
5. The CLI refreshes with Exo and logs out by revoking its Exo session and clearing local credentials. Neither action refreshes or revokes credentials in a vault. Google credentials are never accepted as Exo API bearer tokens or sent to the CLI.

Keep the existing provider client for discovery, PKCE, credential storage, refresh, and account pinning. Any missing registration or revocation support belongs in that code. Keep downstream vault/MCP OAuth unchanged. Browser login uses the same OIDC callback, admission checks, and principal mapping, then establishes its Exo cookie instead of a CLI grant.

The first implementation checkpoint is this complete flow against Google, including refresh after a server restart and logout. Verify wrong PKCE, callback/resource mismatches, code replay, expired tokens, and revoked sessions before expanding the remote API. No new identity/vault state is created for a rejected login.

### Google setup

For local testing, use `http://127.0.0.1:4766` and register `http://127.0.0.1:4766/exo/auth/callback`. HTTP is accepted only for loopback; remote deployments require an HTTPS public URL. A [local config example](../../examples/server-auth/google-local.yaml) is included.

#### 1. Register the server with Google

Choose the server's public HTTPS origin first, such as `https://exo.example.com`. Route it through your HTTPS reverse proxy to Exo's local listener.

In your Google Cloud project, open **Google Auth platform**:

- Configure **Branding** and **Audience**. Use Internal for a deployment limited to your Google Workspace organization, or External for personal Google accounts or users outside that organization. For initial External testing, add your intended accounts under Test users. [Google consent setup](https://developers.google.com/workspace/guides/configure-oauth-consent)
- Under **Clients**, create an OAuth client with application type **Web application**, and register `https://exo.example.com/exo/auth/callback` as an authorized redirect URI. Save the client ID and client secret. [Google web client setup](https://developers.google.com/identity/protocols/oauth2/web-server#creatingcred)

The callback must exactly match the registered URI. Exo derives it from the configured public URL, not the incoming Host header. Its login requests use `openid email profile`; they do not request Drive or other Google API access. [Google OIDC](https://developers.google.com/identity/openid-connect/openid-connect)

Register one Google OAuth client per Exo deployment. Individual users and CLI installations do not need to create Google OAuth clients.

#### 2. Store the Google client secret

`exo serve` defaults to the same `.exo` state directory as the local CLI; no separate state directory or `--config-dir` is needed. Run these commands on the server host from its working directory, using the built-in local provider and default `.exo` state. No provider profile needs to be created. Check `exo provider` first; if a saved profile is selected, clear that selection in the scope it reports before importing the secret. Import the client secret with `GOOGLE_CLIENT_SECRET` set locally to the value copied from Google:

```sh
exo vault create server-auth
exo vault secret create server-auth google-client \
  --token-env GOOGLE_CLIENT_SECRET
unset GOOGLE_CLIENT_SECRET
```

`--token-env` here is the existing one-time vault import option. The running server reads the stored secret through its vault reference. This is Google's application credential, not a user's Google token. The configured auth vault is operator-only: exclude it from automatic attachment and reject explicit agent/thread attachment by name or ID. Enforce this for the resolved vault ID, regardless of its name.

#### 3. Write `server-auth.yaml`

Replace the public URL, client ID, and example email addresses with your values:

```yaml
public_url: https://exo.example.com
oidc:
  issuer: https://accounts.google.com
  client_id: "<client ID copied from Google>"
  client_secret:
    vault: server-auth
    secret: google-client
owner_email: you@gmail.com
allowed_emails: []
```

The owner is always admitted. For a personal deployment, leave `allowed_emails` empty. For a team, list additional admitted users:

```yaml
allowed_emails:
  - teammate@gmail.com
```

Google's audience setting and Exo's admission list both apply. Being able to sign in to Google does not automatically admit someone to Exo.

Email entries are enrollment selectors: require a validated Google identity with verified email, then pin the entry to `(issuer, subject)` on its first admitted login. A different subject must not take over that entry or its vault later, even if Google returns the same email; changing a binding requires a local operator action. Ownership and vault lookup use the pinned identity. Google documents `sub` as the stable account identifier. [Google identity claims](https://developers.google.com/identity/openid-connect/openid-connect#an-id-tokens-payload)

#### 4. Start Exo and log in

Server command, run from the same directory as the vault import:

```sh
exo serve --auth-file ./server-auth.yaml
```

Use the same `--root` on vault and serve commands if choosing a state directory other than `.exo`. A named local provider is only a convenience for saving that path.

Validate the required auth settings and resolve the client-secret reference before accepting requests. Report missing settings or inaccessible secrets directly, and print the expected Google callback URL at startup.

The reverse proxy forwards `https://exo.example.com` to `127.0.0.1:4766`, including streaming responses. Add `--multiplayer` to share agents and threads among admitted users; it does not change the login configuration or share personal vaults.

On each client device:

```sh
exo provider create personal --url https://exo.example.com/exo
exo provider login personal
exo provider switch personal
```

This runs the provider-login flow above: Google identifies the user to Exo, and Exo issues the CLI's own session. The Google client registration is the one-time operator setup; CLI registration happens with Exo. Users do not configure their own Google applications or receive the server's Google client secret.

## Identity-to-vault association

Persist a small record in the server's existing state:

```text
(issuer, subject) -> principal_id, personal_vault_id
```

- After authentication and admission succeed, atomically look up or create the principal and its personal vault. Concurrent first logins must not create duplicate vaults.
- Returning users and additional devices reuse the same association. Display the default vault as `personal`; its stored ID is the authority, not its display name.
- Existing local vaults remain owned by the configured server owner and keep their IDs. They are available for explicit attachment; they are not automatically attached. Rebinding a personal vault to a different existing vault remains a local-operator follow-up.
- New personal vaults start empty. Users add model credentials or connect MCP services once, using the existing vault/OAuth flow.
- Additional vaults created by a user belong to that principal. Optional `shared_vaults: [team]` in the auth config grants use to admitted users; only the owner manages their credentials. Shared vaults must be explicitly attached, and their credentials must have an HTTP or MCP destination to be used by another principal. Raw secret reads and credential management remain with the owner. Auth and personal vaults cannot be shared.

Authentication produces a caller-scoped `VaultContext` containing the personal vault and explicitly granted shared vaults. Attachments and persisted secret references must be checked against that context on every use. A client-supplied vault ID, a shared agent, or a saved thread is not an authorization grant.

Keep `VaultHandle`, secret IDs, encryption, OAuth refresh, destination checks, rotation, revocation, and credential substitution as the implementation. Custom providers and external vault backends can supply their own principal-to-context mapping.

Existing local state needs an explicit owner assignment when enabling auth. Preserve vault/agent/thread IDs and account aliases; make the assignment restart-safe. Do not expose an existing local `global` vault to newly admitted users by default.

## Private and multiplayer execution

Store creator identity on agents and threads. A small server policy checks admission, ownership, and the single `--multiplayer` setting; OSS does not need projects, organizations, or arbitrary ACLs. Server configuration, admission, shared environments, and shared-vault administration remain operator-controlled. Teammates can select a published environment; new host mounts, host-credential filesystem/Git preparation, custom host modules, tool creation, and adapters require the owner.

In private mode, filter lists and enforce ownership on direct lookups, artifacts, event streams, forks, mutation, cancellation, and approvals. Apply checks in the shared runtime/provider boundary so adapters and background execution cannot bypass them.

In multiplayer mode, admitted users can discover and work with shared agents and threads. Every accepted turn records the invoking principal and its authorized vault selection. Only that principal can approve use of its credentials; other viewers cannot approve on its behalf. Cancellation may be shared without granting approval authority.

Do not reuse a credential-bearing warm harness, sandbox proxy binding, or MCP client across principals. When a different user continues a shared thread, invalidate the previous execution context and establish one for the new caller before running. The thread's shared history/files remain visible, but old vault references cannot become ambient credentials. Keep one active turn per thread.

Durable jobs and adapters run under an explicitly recorded principal and vault grant, independent of a browser session's lifetime. Recheck admission and vault access when dispatching. Revocation rejects new work and invalidates affected cached connections; already-sent external requests may finish.

## Minimal web UI

Deferred until the remote CLI workflow is complete. This phase includes only the browser pages needed to complete authentication; agent, thread, chat, and vault management stay in the CLI.

## Implementation sequence and acceptance

1. **Namespace and serving.** `/exo` is the only OSS managed API namespace; move the command to `exo serve` and keep one service implementation. Verify the existing HTTP client against full-provider and agent-filtered servers. The raw `/request` route stays absent.
2. **Embedded login and personal vaults.** Prove the provider-login contract above against direct Google/OIDC from one Exo process, using the existing CLI. Cover browser login, session refresh/logout across restart, stable identity, atomic vault provisioning, and explicit adoption of existing local state. Verify that two devices get the same vault, two identities get different vaults, and rejected identities create no state.
3. **Authorized provider operations.** Finish remote vault CRUD and private-mode enforcement using caller-scoped handles. Exercise agents, threads, artifacts, history/SSE, cancellation, approvals, environments, and vault operations as two users, including direct-ID requests and revoked access. Keep this in one enforcement path shared with non-HTTP execution.
4. **Multiplayer.** Add the server flag and exercise two users continuing the same thread. Verify shared history, separate credential selection, caller-only approvals, revoked grants, and no credential/session reuse across principals. Test restart, background jobs, and switching sharing mode without losing creator metadata.
5. **CLI end-to-end demo.** Sign in from a second device, reconnect to a saved turn, approve/cancel, connect Notion in a personal vault, rotate/revoke it, and repeat with a second user in both modes. Verify that the auth vault cannot be attached automatically or explicitly.

Ship these as separate reviewable changes. Automated validation covers the shared local/HTTP CLI workflows, OAuth discovery/login/refresh/revocation, browser CSRF protection, concurrent enrollment, shared-vault rotation/revocation, caller switching, and owner-only host configuration. A live Google login through `exo provider login` and authenticated `exo agent list` have passed. The two-user live Notion exercise remains to be done; the deferred web UI is not a release requirement.

## References

- [Omnigent auth and SSO](https://omnigent.ai/docs/collaborate/auth) and [login routes](https://github.com/omnigent-ai/omnigent/blob/main/omnigent/server/routes/auth.py): embedded login and operator-controlled admission; Exo retains its existing CLI OAuth contract.
- [`openidconnect`](https://docs.rs/openidconnect/latest/openidconnect/) and [`oxide-auth`](https://docs.rs/oxide-auth/latest/oxide_auth/): Rust OIDC client and OAuth server library candidates; validate coverage in the first login checkpoint.
- [OIDC stable identity](https://openid.net/specs/openid-connect-core-1_0.html#ClaimStability): use issuer and subject for identity.
- Existing code: `crates/cli/src/serve.rs`, `crates/cli/src/providers/oauth.rs`, `crates/executor/src/http_service.rs`, and `crates/exoharness/src/vault.rs`.
