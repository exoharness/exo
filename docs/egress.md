# Sandbox egress

Sandbox policy is part of `SandboxSpec`. Each backend enforces the policy when
it acquires, attaches, or restores a sandbox, before returning a usable handle.
Unsupported policies fail with an error identifying the unsupported field.

The existing `firecracker` build feature includes the proxy; there is no
separate egress feature to enable. Firecracker implements credential
substitution with a transparent HTTP/HTTPS proxy. Programs receive placeholder
environment variables; the proxy resolves credentials outside the VM and
substitutes them on authorized requests.

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
exo secret set notion --env NOTION_API_KEY
exo --egress-policy egress.json sandbox play \
  --provider firecracker --networking enabled --idle-seconds 300
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

The CLI interprets binding names as names or IDs in the local encrypted secret
store. Names resolve in the nearest scope: thread, then agent, then global. IDs
must belong to one of those scopes. Missing, ambiguous, or non-key secrets fail
the request. The same flag works with `exo repl` and a managed Firecracker
sandbox. Tell the agent which variables it can use; the runtime currently
injects the environment without adding a credential inventory to its prompt.

## Policy and credentials

`SandboxNetworkPolicy` controls where the sandbox can connect. Each binding's
`CredentialNetworkPolicy` separately controls where substitution is permitted.
Both must allow the destination. An unrestricted credential inherits the
sandbox's permitted destinations.

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
resolved again, so rotation and revocation take effect without replacing the
sandbox. Identity includes the sandbox ID and agent/thread scope; destination
includes the host, port, method, and normalized path/query. A vault adapter can
pin a binding to a vault/secret reference per thread and enforce its stored
destination restrictions. The local CLI resolver assumes a single user owns the
secret store; hosted resolvers must supply their own authorization. Resolver
failures are sanitized before returning them to the guest.

```rust
let backend = firecracker_backend_with_credentials(config, lima, resolver).await?;
request.spec.policy = policy;
let sandbox = backend.acquire(request).await?;
let output = sandbox.exec(&command).await?;
```

The sandbox handle injects placeholders into `exec`, `start_process`, and terminals,
overriding caller-supplied values. It also configures TLS trust for curl, Git,
Python requests, Node, and clients that use `SSL_CERT_FILE`. Images need
`/bin/sh`, `cat`, and `/etc/ssl/certs/ca-certificates.crt`. Separate trust stores
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

Docker, Apple Containers, smolvm, E2B, and Daytona retain their existing enabled /
disabled networking support. They reject limited networking and credential
bindings. Local processes, Sprites, and AWS AgentCore only accept unrestricted
networking. Docker attachments also reject disabled networking because Exo does
not control the attached container's network.

## Current scope

The proxy supports limited networking with exact hosts and HTTPS header
substitution. Unrestricted networking with credential bindings is rejected until
passthrough is implemented. Body substitution is not part of the policy yet.
Standard ports 80/443 are supported; local gateways on other ports need
additional transport support. Model credential bindings are not inferred
automatically.

Firecracker proxy policies do not yet support snapshots/forks, external
attachments, or one-shot sandboxes. HTTP/2, WebSockets, arbitrary TCP, and signed
requests are also outside this initial implementation.

## Git over HTTPS

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

For Basic authentication, the resolver returns the Base64 payload expected after
`Basic`, and the proxy substitutes it for the placeholder. `Git-Protocol: version=2`
passes through unchanged, and `GIT_SSL_CAINFO` provides trust for the
proxy's certificate. Redirects are not followed. Request bodies over 8 MiB are
rejected, so larger push packfiles need additional support. Repository and
operation authorization remain the caller's responsibility; Exo resolves the
selected binding and forwards only requests the resolver authorizes.

## Tests

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
