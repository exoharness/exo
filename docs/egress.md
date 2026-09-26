# Sandbox egress

Sandbox policy is part of `SandboxSpec`. Each backend enforces the policy when
it acquires, attaches, or restores a sandbox, before returning a usable handle.
Unsupported policies fail with an error identifying the unsupported field.

The basic runtime includes the credential proxy; the `egress` feature also exposes
it without a VM backend. Programs receive placeholder
environment variables; the proxy resolves credentials outside the sandbox and
substitutes them on authorized requests. Firecracker uses transparent network
interception. Apple container and Docker use an authenticated HTTP CONNECT
transport into the same credential resolver and request forwarding code.

On macOS, TLS and credential resolution run in the native Exo process. The
existing Lima bridge carries streams and DNS configuration, without receiving
credentials or the TLS signing key.

## Try it

Build with `cargo build -p exo --features firecracker` and save this
as `egress.json` (the flag also accepts `.yaml`, `.yml`, and `.toml`):

```json
{
  "networking": {
    "type": "limited",
    "allowed_hosts": ["api.notion.com"]
  },
  "credentials": [
    {
      "name": "notion",
      "environment_variable": "NOTION_API_KEY",
      "networking": {
        "type": "limited",
        "allowed_hosts": ["api.notion.com"]
      },
      "injection_location": { "header": true }
    }
  ]
}
```

```bash
exo vault secret create global notion \
  --http-origin https://api.notion.com --token-env NOTION_API_KEY
exo sandbox --egress-policy egress.json play \
  --sandbox firecracker --networking enabled --idle-seconds 300
```

Inside the sandbox:

```bash
curl https://api.notion.com/v1/users/me \
  -H "Authorization: Bearer $NOTION_API_KEY" \
  -H "Notion-Version: 2022-06-28"
```

The client supplies `Bearer ` or other surrounding syntax. The proxy replaces
only the placeholder. `Authorization`, `x-api-key`, and other ordinary headers
work; routing and framing headers cannot contain placeholders.

The CLI selects binding names or IDs from the sandbox's accessible vaults:
thread attachments, then agent attachments, then the global vault. Named vaults
must be attached before their secrets can be selected. The selected vault and
secret IDs are saved with the sandbox. Rotation takes effect on the next request;
removing a secret or vault fails the request, even if another vault has a secret
with the same name. Recreating a secret requires a new sandbox selection.

HTTP credentials authorize one exact HTTPS origin, including its port. The
`--http-origin` grant allows header substitution on requests to that origin;
MCP credentials authorize only their exact MCP endpoint, including path and query.
Keys without a destination do not grant HTTP access.
Sandbox and credential host policies still apply. The same policy flag
works with `exo chat` and a managed Firecracker sandbox. Tell the agent which
variables it can use; the runtime currently injects the environment without
adding a credential inventory to its prompt.

For a managed agent, use `exo chat --agent-file agent.md --sandbox firecracker
--vault alice --egress-policy egress.json` to attach a named vault to the thread.
`--agent-file` keeps the agent, thread, and sandbox selection temporary while
credential rotation still reads the live vault.

## Agent model credentials

The built-in Codex, Claude Code, and Pi wrappers use the registered model binding
when creating their sandbox. Register the key and model once:

```bash
exo vault secret create global openai --token-env OPENAI_API_KEY
exo model create gpt-5-mini --secret openai
exo chat --agent-file exoharness/examples/managed-agents/pi-assistant.md --environment-file exoharness/examples/environments/pi-local.yaml
```

The runtime adds the model credential to the selected environment's policy. The
wrapper reads model metadata; it does not read or forward the vault key. The
proxy injects a placeholder into the SDK's usual API-key environment variable.
Codex uses its provider's environment-key setting and needs no login file.
Model authentication currently supports API keys; OAuth/subscription login
caches are not passed into the sandbox.
Its provider is configured for HTTP streaming, which the proxy supports.

The model binding pins the vault and secret IDs. Same-named secrets in attached
vaults cannot substitute another account. Keys without a destination are
permitted through the model binding only at its registered HTTPS endpoint and
base path. Keys with an explicit destination must match that endpoint. MCP OAuth
credentials cannot be used as model API keys. Rotation is read on every request;
deleting the selected secret revokes it, including after a CLI restart. Recreating
its name does not restore the old binding.

An existing environment credential for the model's environment variable must
select the same secret and permit its endpoint. Conflicts fail before startup.
Limited network policies must already allow the model host; model selection does
not expand them. Model endpoints currently require HTTPS on port 443.

Apple container and Docker support model credentials with
`networking: { type: unrestricted }`. The runtime supplies `HTTPS_PROXY`, HTTP
proxy authentication, and the CA bundle. Node clients use `NODE_USE_ENV_PROXY=1`
(requires Node 22.21 or later; the supplied images include it). This protects
credentials while leaving the sandbox's general networking unrestricted. These
backends still reject limited networking. Firecracker enforces limited networking
through its existing transparent proxy and requires an explicit host allowlist.

The authenticated proxy listens only on the sandbox network's host gateway
(Apple container and native Docker), or host loopback (Docker Desktop). Its
credentials authorize the sandbox's selected endpoints for the proxy session.

Clients must restart after the runtime restarts to receive a fresh proxy session.
Exo launches the wrappers with the current session when resuming a saved thread.
Proxy credentials grant use of the selected model endpoint while the sandbox is
active; they do not prevent sandbox code from making its own authorized model
requests.

## Native MCP credentials

Codex and Claude Code use their native MCP clients for servers declared in the
agent definition. Exo supplies the server URLs and tool filters, reusing the
thread's saved vault selection. Basic, RLM, and Pi retain the host MCP tool bridge.

Authenticated native servers receive an `EXO_MCP_...` environment variable with a
placeholder. The existing proxy resolves the selected vault secret for each
request, restricted to the exact MCP endpoint. Rotation and revocation apply to
saved threads without switching accounts. OAuth expiry refresh happens in the
runtime; after an upstream 401, the proxy refreshes and retries once if the token
changed, preserving the request body and MCP session header. It does not retry
403 responses. Refreshed tokens are saved in the vault.

Authenticated MCP endpoints require HTTPS on port 443. A limited environment
network policy must already allow each MCP host; declaring a server does not
expand the network policy. Public servers need no credential.

Native MCP permission hooks call the shared Harness authorization mechanism.
The same CLI allow/deny responder works locally and over HTTP, using canonical
`exo_mcp__<server>__<tool>` names. Codex supports these MCP approvals but rejects
default/native `always_ask` policies because its hooks do not cover every native
tool. Codex resource listing, resource-template listing, and resource reads do not
request per-tool approval. Declared MCP tool calls use the configured tool policies.
Tool approvals control harness calls; credential grants authorize requests
to the endpoint and do not enforce tool-level permissions on arbitrary sandbox
code.

## Policy and credentials

`SandboxNetworkPolicy` controls where the sandbox can connect. Each binding's
`CredentialNetworkPolicy` separately controls where substitution is permitted.
Every credential must specify `networking: { "type": "limited", "allowed_hosts": [...] }`.
Both allowlists must contain the destination: allowing the sandbox to reach
`api.notion.com` and `api.github.com` does not permit a Notion credential restricted
to `api.notion.com` to be sent to GitHub. Expanding the sandbox's allowlist never
expands a credential's allowed destinations. An empty credential allowlist
disables substitution for that credential.

Credential policies using `"type": "unrestricted"` are rejected and must be updated
to list their allowed hosts explicitly. They do not inherit the sandbox's allowlist.

Binding names are scoped references, not storage IDs. Two threads can both
request `notion` and resolve different secrets. Implement `EgressCredentialResolver`:

```rust
async fn resolve(
    &self,
    identity: &EgressIdentity,
    binding_name: &str,
    destination: &EgressDestination,
) -> anyhow::Result<String>;
```

The caller selects bindings in `request.spec.policy.credentials`. Each use is
resolved again, so rotation and revocation take effect without replacing the sandbox. Identity
includes the sandbox ID and `ResourceScope`; destination includes the host,
port, method, and normalized path/query. The local resolver loads the saved
vault/secret reference and checks the current scope before each use.
`egress::vault::resolve_credential` resolves that reference through `VaultHandle`
with the matching HTTP or MCP target. Hosted resolvers can use the same helper with their
authenticated vault context. The local CLI assumes one user owns its vault
catalog. Resolver failures are sanitized
before returning them to the guest.

```rust
let backend = firecracker_backend_with_credentials(config, lima, resolver).await?;
request.spec.policy = policy;
let sandbox = backend.acquire(request).await?;
let output = sandbox.exec(&command).await?;
```

The sandbox handle injects placeholders into `exec`, `start_process`, and terminals,
overriding caller-supplied values. It also configures TLS trust for curl, Git,
Python requests, Node, and clients that use `SSL_CERT_FILE`. Images need
`/bin/sh`, `cat`, `mktemp`, `mv`, `rm`, and `/etc/ssl/certs/ca-certificates.crt`. Separate trust stores
need their own integration. Callers must avoid passing other real credentials
or credential files into the sandbox.

`EgressTransport` provides source-bound listeners. Firecracker redirects guest
TCP 80/443 and TCP/UDP 53, checks the guest source against its veth, and rejects
other traffic, including IPv6 and QUIC. CIDR exceptions cannot bypass the proxy.
DNS answers exact allowed names with a synthetic IPv4 address. The proxy resolves
and pins public upstream IPv4 addresses, validates TLS, requires matching SNI
and HTTP Host, and does not follow redirects. Requests are bounded to 8 MiB;
responses stream, including SSE. Allowed upstreams can still return sensitive
values in their responses.

`CreateSandboxRequest.policy` supplies the policy through the Exoharness API.
`BasicExoHarnessConfig.sandbox_policy` supplies a default, including for the
CLI's `--egress-policy`. A selected policy takes precedence over the older
`enable_networking` flag; without a policy that flag still applies. New CLI
agents enable networking by default. The selected policy is persisted with the
sandbox and included in its spec hash. New records store only the policy; legacy
records with `enable_networking` remain readable. The event's legacy boolean is
derived from the policy. Changing the default does not rewrite existing
sandboxes. Binding values and proxy listener addresses are never in the policy.

The Firecracker backend creates its listeners during acquisition. On Linux,
`FirecrackerConfig.egress_listen` can control where they bind and which address
the VM uses to reach them:

```rust
config.egress_listen = Some(EgressListenConfig {
    bind_address: "0.0.0.0".parse()?,
    advertised_address: "10.0.0.10".parse()?,
    http_port: 0,
    https_port: 0,
    dns_port: 0,
});
```

Use an address routed from the guest network. Zero ports allocate dynamically;
fixed ports must be unique for each active sandbox on that address. When binding
to `0.0.0.0`, the advertised address must be assigned locally: DNS binds to that
address so UDP replies have the source address the guest expects. With
Lima, this configuration applies inside the Linux VM. Without an explicit
configuration, the local transport selects the host's routed IPv4 address.

The Firecracker and Lima backends own their proxies. `shutdown_egress()` closes
all proxies while retaining the VMs, including when another acquisition is
pending. A fresh backend can reacquire it with new listeners, trust, and
placeholders; existing client processes must restart to receive those values. On
Firecracker (including Lima), `stop` flushes durable filesystems before closing
egress or stopping the VM. If the flush fails, the VM and its networking remain
available for retry. `terminate` revokes egress and attempts the flush, but logs
a sync failure or timeout and continues destroying the VM. Both release listener
ports even while callers retain old handles. VM cleanup, including idle reaping,
closes its listeners before releasing its network address. This also applies to
the listeners inside the Lima bridge. Low-level callers that already own their
proxy can pass endpoints to
`FirecrackerSandboxBackend::acquire_request(FirecrackerRequest)`.

`EgressProxy::start(identity, policy, resolver, transport, cancel)` starts the
proxy using the existing `EgressPolicy` and an `EgressTransport`. It exposes
listener endpoints, the CA certificate, and placeholder environment variables.

`ExplicitProxy::start(identity, policy, resolver, listener, advertised_host)`
serves an authenticated HTTP/HTTPS proxy on a caller-bound `TcpListener`. It
creates the CA and credential placeholders once per proxy. Install `ca_pem` at
`ca_path` inside the sandbox and apply `environment` (or use `command()`) to set
`HTTPS_PROXY`, placeholder credentials, and CA trust variables. `close()` or
Drop closes the listener and active tunnels. The caller owns sandbox setup;
this API does not change sandbox backend integration.

Limited networking checks every request against the sandbox's allowed hosts,
including anonymous requests. With unrestricted networking, the explicit proxy
intercepts credential hosts and tunnels other HTTPS destinations unchanged.
Proxy configuration alone does not prevent clients from bypassing the proxy;
network isolation requires separate sandbox routing or firewall enforcement.

For a shared hosted listener, call
`serve_connect_proxy(listener, authorizer, shutdown)`. Implement `ProxyAuthorizer`
to validate the Basic proxy username/password and return the authorized
`ProxySession` for the requested host. Construct sessions with
`ProxySession::new(identity, policy, resolver, tls, placeholders)` using TLS
material trusted by that sandbox and its stable credential placeholders. Sessions
can be cached and cloned; authentication still runs for every CONNECT request.
Returning `None` rejects authentication with 407; errors or a 10-second timeout
return 503. Exo validates CONNECT framing, enforces the session's egress policy,
and owns connection limits, upgrades, and shutdown. The hosted listener accepts
only CONNECT on port 443. `ExplicitProxy` uses the same server with a fixed
sandbox session and generated password, and also accepts plain HTTP requests.

Callers that already own an authenticated CONNECT listener can instead pass its
post-200 stream to `serve_https_connect` with the CONNECT authority, TLS acceptor,
and stable placeholders. Exo checks CONNECT host, SNI, and HTTP Host before
forwarding. Session selection and credential resolution are separate: the
hosted caller must authenticate access to the sandbox even when the request
uses no credential.

A hosted backend can implement `ManagedSandboxBackend::acquire` itself: choose
the sandbox node, establish an authenticated relay to the credential service,
install routing, and only then return a handle. No proxy hooks are required on
the shared sandbox traits. The credential service can run inside Loop while
the node relays opaque streams. Relay authorization must bind the stream to the
sandbox allocation and its generation; raw source-IP binding is only suitable
before NAT on a trusted local host. The production relay is not implemented here.

## Other backends

Vercel translates unrestricted, disabled, and exact-host policies to its native
network policy. Exact-host rules pin the HTTP Host header. Acquisition updates
an existing sandbox's policy before resuming its session; a failed update
prevents resume. It skips the update when the returned policy exactly matches
and skips resume when the session is already running. Vercel hides injected
header values, so exact-host pinning must be reapplied. Placeholder credential
bindings are rejected: native header transforms set whole values and do not
implement Exo's per-request credential resolution. A Vercel forwarding adapter
would be a separate implementation.

Docker and Apple Containers support credential bindings on managed sandboxes
with unrestricted networking. They reject limited networking, credential-protected
attachments, and restoring credential-protected snapshots. Credential proxies
close when the sandbox stops or the runtime exits. Smolvm, E2B, and Daytona retain
their enabled / disabled networking support and reject credential bindings and
limited networking. Local processes, Sprites, and AWS AgentCore only accept unrestricted
networking. Docker attachments also reject disabled networking because Exo does
not control the attached container's network.

## Current scope

The transparent proxy supports limited networking with exact hosts and HTTPS
header substitution. The container CONNECT transport supports credential
substitution with unrestricted networking; other public HTTPS destinations pass
through without interception. Firecracker still rejects unrestricted networking
with credential bindings. Body substitution is not part of the policy yet.
Standard ports 80/443 are supported; local gateways on other ports need
additional transport support.

Firecracker proxy policies do not yet support snapshots/forks, external
attachments, or one-shot sandboxes. HTTP/2, WebSockets, arbitrary TCP, and signed
requests are also outside this initial implementation.

## Git over HTTPS

Git can use its normal HTTPS URL through `HTTPS_PROXY`. For Basic authentication,
Exo decodes the username/password, substitutes any credential placeholder with
the token returned by the resolver, and re-encodes the header before forwarding it.

Git read and write operations use ordinary smart HTTP. A read uses
`GET /repo.git/info/refs?service=git-upload-pack` followed by
`POST /repo.git/git-upload-pack`; a write uses
`GET /repo.git/info/refs?service=git-receive-pack` followed by
`POST /repo.git/git-receive-pack`. These paths are forwarded when the caller's
`EgressCredentialResolver` authorizes the destination and operation. A binding's
environment variable is injected as an `exo_egress_...` placeholder inside the
sandbox. Configure it with Git's `http.<url>.extraHeader` setting:

```bash
git -C "$repo" config --local \
  'http.https://git.example.com/.extraHeader' \
  "Authorization: Basic $GIT_AUTH"
```

For this raw-header configuration, the resolver returns the Base64 payload
expected after `Basic`, and the proxy substitutes it for the placeholder. `Git-Protocol: version=2`
passes through unchanged, and `GIT_SSL_CAINFO` provides trust for the
proxy's certificate. Redirects are not followed. Request bodies over 8 MiB are
rejected, so larger push packfiles need additional support. Repository and
operation authorization remain the caller's responsibility; Exo resolves the
selected binding and forwards only requests the resolver authorizes.

## Tests

Run the shared proxy tests without a VM backend:

```bash
cargo test -p exoharness --features egress --lib egress::
```

With the [Firecracker artifacts](../support/firecracker/README.md) installed:

```bash
cargo test -p exoharness --features firecracker --lib
bash support/firecracker/egress-smoke.sh
bash support/firecracker/managed-egress-smoke.sh
```

The first smoke test checks two VMs against a controlled TLS upstream, including
host/SNI and DNS denial, direct egress, credential rotation/removal, isolation,
and shutdown. The managed test covers automatic trust, process environments,
and reacquiring the same VM with fresh bindings. On macOS it crosses the real
Lima bridge with an isolated test executable. Set
`EXO_FIRECRACKER_LIMA_INSTANCE` to use a different Lima instance.

The macOS agent workflow tests run Codex, Claude Code, and Pi through both the
local CLI and HTTP runtime, including saved-thread resume and credential
inspection inside the guest. It requires `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`,
and `/var/lib/exo/firecracker/rootfs.ext4` containing the pinned binaries from
the three sandbox images. Set `EXO_EGRESS_BRIDGE_BINARY` to a prebuilt bridge
inside Lima to reuse it:

```bash
cargo test -p exo --features firecracker --test container_live \
  firecracker_ -- --ignored --nocapture --test-threads=1
```
