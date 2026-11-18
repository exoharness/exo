//! HTTP/TLS handling and credential substitution outside the sandbox.
//!
//! `EgressTransport` supplies connections from the sandbox network. This module
//! checks destinations, resolves credentials, and forwards requests. `sandbox`
//! coordinates proxy setup with sandbox acquisition; `transport`
//! implements the listeners and DNS service on the VM host.

use std::collections::{HashMap, HashSet};
use std::convert::Infallible;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, ensure};
use async_trait::async_trait;
use base64::Engine;
use bytes::Bytes;
use futures::TryStreamExt;
use http_body_util::{BodyExt, Full, Limited, StreamBody, combinators::BoxBody};
use hyper::body::{Frame, Incoming};
use hyper::header::{CONNECTION, HOST, HeaderMap, HeaderValue};
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::{TokioIo, TokioTimer};
use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, Issuer, KeyPair, KeyUsagePurpose};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite};
use tokio::sync::{Semaphore, mpsc, oneshot};
use tokio::task::{AbortHandle, Id, JoinSet};
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls::{self, ServerConfig, pki_types::PrivatePkcs8KeyDer};
use tokio_util::sync::CancellationToken;

use crate::types::canonical_egress_host;

use crate::{EgressPolicy, SandboxEgressProxy};

mod external;
pub use external::ExternalProxyConfig;
mod explicit;
pub use explicit::{ExplicitProxy, ProxyAuthorizer, ProxySession, serve_connect_proxy};
mod redirect;
pub use redirect::EgressRedirectPolicy;
mod transport;
pub use crate::egress_credentials::vault;
#[cfg(feature = "firecracker")]
pub(crate) use transport::NetworkDns;
pub use transport::{EgressTransport, LocalEgressTransport};
mod sandbox;
pub(crate) use sandbox::{AcquisitionMode, EgressRuntime, SandboxEgress, SandboxProxy};

use crate::egress_credentials::{Binding, strip_hop_headers};
pub use crate::egress_credentials::{
    EgressCredentialResolver, EgressDestination, EgressError, EgressIdentity,
};
pub(crate) const IO_TIMEOUT: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_MAX_REQUEST_BODY_BYTES: usize = 128 * 1024 * 1024;
const HTTP_BUFFER_SIZE: usize = 32 * 1024;
const MAX_CONNECTIONS: usize = 128;

pub(crate) struct ResolvedUpstream {
    pub(crate) addresses: Vec<SocketAddr>,
    pub(crate) root_certificate: Option<reqwest::Certificate>,
}

#[async_trait]
pub(crate) trait UpstreamResolver: Send + Sync {
    async fn resolve(&self, host: &str, port: u16) -> Result<ResolvedUpstream>;
}

pub(crate) struct PublicUpstreamResolver;

#[async_trait]
impl UpstreamResolver for PublicUpstreamResolver {
    async fn resolve(&self, host: &str, port: u16) -> Result<ResolvedUpstream> {
        // Resolve on every request and discard private/special-use addresses.
        // client() pins these answers, preventing a second DNS lookup from
        // turning an allowed hostname into a connection to an internal service.
        let addresses =
            tokio::time::timeout(CONNECT_TIMEOUT, tokio::net::lookup_host((host, port)))
                .await??
                .filter(|address| public_ipv4(address.ip()))
                .collect::<Vec<_>>();
        ensure!(
            !addresses.is_empty(),
            "upstream has no permitted IPv4 address"
        );
        Ok(ResolvedUpstream {
            addresses,
            root_certificate: None,
        })
    }
}

type ProxyError = Box<dyn std::error::Error + Send + Sync>;
type ProxyBody = BoxBody<Bytes, ProxyError>;

// Owns one sandbox's TLS server, placeholders, and active proxy connections.
// It does not create VMs or decide where credentials are stored.
pub struct EgressProxy {
    endpoints: SandboxEgressProxy,
    transport: Arc<dyn EgressTransport>,
    connection: ProxyConnection,
}

pub(crate) struct ProxyConnection {
    ca_pem: String,
    environment: Mutex<HashMap<String, String>>,
    updates: mpsc::Sender<(State, oneshot::Sender<Result<()>>)>,
    cancel: CancellationToken,
    task: tokio::task::JoinHandle<()>,
}

struct Connections {
    state: Arc<State>,
    active: HashMap<Id, ActiveConnection>,
}

struct ActiveConnection {
    destinations: HashSet<ConnectionDestination>,
    abort: AbortHandle,
}

#[derive(Eq, PartialEq, Hash)]
struct ConnectionDestination {
    host: String,
    port: u16,
    opaque: bool,
}

#[derive(Clone)]
struct ConnectionState {
    connections: Arc<Mutex<Connections>>,
    id: Id,
}

#[derive(Clone)]
enum SessionState {
    Fixed(Arc<State>),
    Live(ConnectionState),
}

impl From<Arc<State>> for SessionState {
    fn from(state: Arc<State>) -> Self {
        Self::Fixed(state)
    }
}

impl SessionState {
    fn snapshot(&self) -> Arc<State> {
        match self {
            Self::Fixed(state) => state.clone(),
            Self::Live(connection) => connection
                .connections
                .lock()
                .expect("egress connections poisoned")
                .state
                .clone(),
        }
    }

    fn destination(&self, host: &str, port: u16, opaque: bool) -> Result<Arc<State>> {
        let Self::Live(connection) = self else {
            return Ok(self.snapshot());
        };
        let mut connections = connection
            .connections
            .lock()
            .expect("egress connections poisoned");
        let state = connections.state.clone();
        state.check_port(port)?;
        ensure!(state.network.permits_host(host), "host is not allowed");
        ensure!(
            !opaque || state.network.unrestricted(),
            "opaque TCP requires unrestricted networking"
        );
        connections
            .active
            .get_mut(&connection.id)
            .context("egress connection is closed")?
            .destinations
            .insert(ConnectionDestination {
                host: host.to_owned(),
                port,
                opaque,
            });
        Ok(state)
    }

    fn request(
        &self,
        request: &Request<Incoming>,
        sni: Option<&str>,
    ) -> Result<Arc<State>, EgressError> {
        let state = self.snapshot();
        let (destination, _) = state.destination(request, sni)?;
        Ok(self.destination(&destination.host, destination.port, false)?)
    }
}

struct PooledClient {
    addresses: Vec<SocketAddr>,
    client: reqwest::Client,
}

// Request handling shared by all connections to one sandbox's proxy. Clients
// are pooled by destination and replaced whenever its resolved addresses change.
pub(crate) struct State {
    external: Option<ExternalProxyConfig>,
    max_request_body_bytes: usize,
    clients: Mutex<HashMap<(String, u16), PooledClient>>,
    hosts: HashSet<String>,
    network: crate::egress_credentials::NetworkPolicy,
    bindings: Vec<Binding>,
    identity: EgressIdentity,
    resolver: Option<Arc<dyn EgressCredentialResolver>>,
    redirect_policy: Option<Arc<dyn EgressRedirectPolicy>>,
    upstream: Arc<dyn UpstreamResolver>,
}

impl EgressProxy {
    pub async fn start(
        identity: EgressIdentity,
        policy: EgressPolicy,
        resolver: Option<Arc<dyn EgressCredentialResolver>>,
        transport: Arc<dyn EgressTransport>,
        cancel: CancellationToken,
    ) -> Result<Self> {
        let state = State::new(identity, policy, resolver, Arc::new(PublicUpstreamResolver))?;
        Self::start_with_transport(transport, state, cancel).await
    }

    async fn start_with_transport(
        transport: Arc<dyn EgressTransport>,
        state: State,
        cancel: CancellationToken,
    ) -> Result<Self> {
        let endpoints = transport.endpoints();
        endpoints.validate()?;
        let incoming = transport.clone();
        let cleanup = transport.clone();
        let connection = ProxyConnection::start(
            state,
            cancel,
            move |_| {
                let transport = incoming.clone();
                async move {
                    let connection = tokio::select! {
                        stream = transport.accept(false) => stream.map(Connection::Http),
                        stream = transport.accept(true) => stream.map(Connection::Https),
                    }?;
                    Ok(std::future::ready(Ok(connection)))
                }
            },
            move || cleanup.close(),
        )?;
        Ok(Self {
            endpoints,
            transport,
            connection,
        })
    }

    pub async fn bind_source(&self, source_ip: Ipv4Addr) -> Result<()> {
        self.transport.bind_source(source_ip).await
    }

    pub fn endpoints(&self) -> SandboxEgressProxy {
        self.endpoints
    }

    pub fn ca_pem(&self) -> &str {
        &self.connection.ca_pem
    }

    pub fn environment(&self) -> HashMap<String, String> {
        self.connection
            .environment
            .lock()
            .expect("egress environment poisoned")
            .clone()
    }

    pub fn close(&self) {
        self.connection.close();
        self.transport.close();
    }
}

/// Serve a CONNECT tunnel after the caller has authenticated it and sent 200.
/// The caller supplies TLS configuration trusted by the sandbox and the same
/// placeholders it installed there. TLS must negotiate HTTP/1.1. No listener
/// or sandbox session is retained.
pub async fn serve_https_connect<T>(
    stream: T,
    connect_authority: &str,
    tls: TlsAcceptor,
    identity: EgressIdentity,
    policy: EgressPolicy,
    resolver: Option<Arc<dyn EgressCredentialResolver>>,
    placeholders: &HashMap<String, String>,
) -> Result<()>
where
    T: AsyncRead + AsyncWrite + Send + Unpin + 'static,
{
    let session = ProxySession::new(identity, policy, resolver, tls, placeholders)?;
    let host = session.state.connect_host(connect_authority)?;
    https_connection(stream, session.tls, session.state, Some(&host)).await
}

impl Drop for EgressProxy {
    fn drop(&mut self) {
        self.close();
    }
}

impl State {
    fn preserves(
        &self,
        next: &Self,
        destination: &ConnectionDestination,
        same_resolver: bool,
    ) -> bool {
        if next.check_port(destination.port).is_err()
            || !next.network.permits_host(&destination.host)
            || (destination.opaque && !next.network.unrestricted())
        {
            return false;
        }
        let relevant = |binding: &&Binding| binding.permits_host(&destination.host);
        let unchanged = |old: &Binding, new: &Binding| {
            old.config == new.config
                && old.placeholder == new.placeholder
                && self
                    .external
                    .as_ref()
                    .and_then(|proxy| proxy.credential_authorizations.get(&old.config.name))
                    == next
                        .external
                        .as_ref()
                        .and_then(|proxy| proxy.credential_authorizations.get(&new.config.name))
                && same_resolver
        };
        self.bindings.iter().filter(relevant).all(|old| {
            next.bindings
                .iter()
                .filter(relevant)
                .any(|new| unchanged(old, new))
        }) && next.bindings.iter().filter(relevant).all(|new| {
            self.bindings
                .iter()
                .filter(relevant)
                .any(|old| unchanged(old, new))
        })
    }

    fn environment(&self) -> HashMap<String, String> {
        let mut environment = self
            .external
            .as_ref()
            .map(|proxy| proxy.environment.clone())
            .unwrap_or_default();
        environment.extend(self.bindings.iter().map(|binding| {
            (
                binding.config.environment_variable.clone(),
                binding.placeholder.clone(),
            )
        }));
        environment
    }

    pub(crate) fn new(
        identity: EgressIdentity,
        policy: EgressPolicy,
        resolver: Option<Arc<dyn EgressCredentialResolver>>,
        upstream: Arc<dyn UpstreamResolver>,
    ) -> Result<Self> {
        Self::new_with_placeholders(identity, policy, resolver, upstream, None, None)
    }

    fn new_with_placeholders(
        identity: EgressIdentity,
        policy: EgressPolicy,
        resolver: Option<Arc<dyn EgressCredentialResolver>>,
        upstream: Arc<dyn UpstreamResolver>,
        placeholders: Option<&HashMap<String, String>>,
        external: Option<ExternalProxyConfig>,
    ) -> Result<Self> {
        let placeholders = external
            .as_ref()
            .map(|proxy| &proxy.environment)
            .or(placeholders);
        if let Some(proxy) = &external {
            proxy.validate()?;
        }
        ensure!(
            !identity.sandbox_id.is_empty(),
            "egress identity is required"
        );
        let network = crate::egress_credentials::NetworkPolicy::new(&policy)?;
        // Allowed hosts also determine TLS interception; unrestricted traffic
        // needs certificates only for destinations with credential bindings.
        let mut hosts = network.hosts().clone();
        if network.unrestricted() {
            for binding in &policy.credentials {
                hosts.extend(binding.networking.hosts()?);
            }
        }
        ensure!(
            policy.credentials.is_empty() || resolver.is_some() || external.is_some(),
            "credential substitution requires an egress credential resolver"
        );
        if external.is_none()
            && let Some(placeholders) = placeholders
        {
            ensure!(
                placeholders.len() == policy.credentials.len(),
                "placeholder map must match credential bindings"
            );
        }
        let bindings =
            crate::egress_credentials::prepare_bindings(policy.credentials, placeholders)?;
        Ok(Self {
            external,
            max_request_body_bytes: match std::env::var("EXO_EGRESS_MAX_REQUEST_BODY_BYTES") {
                Ok(value) => value.parse().context(
                    "EXO_EGRESS_MAX_REQUEST_BODY_BYTES must be a non-negative integer in bytes",
                )?,
                Err(std::env::VarError::NotPresent) => DEFAULT_MAX_REQUEST_BODY_BYTES,
                Err(error) => {
                    return Err(error).context("reading EXO_EGRESS_MAX_REQUEST_BODY_BYTES");
                }
            },
            clients: Mutex::new(HashMap::new()),
            hosts,
            network,
            bindings,
            identity,
            resolver,
            redirect_policy: None,
            upstream,
        })
    }

    pub(crate) fn check_port(&self, port: u16) -> Result<()> {
        self.network.check_port(port)
    }

    pub(crate) async fn connect_tcp(
        &self,
        destination: SocketAddr,
    ) -> Result<tokio::net::TcpStream> {
        ensure!(
            self.network.unrestricted(),
            "opaque TCP requires unrestricted networking"
        );
        self.check_port(destination.port())?;
        ensure!(
            public_ipv4(destination.ip()),
            "TCP destination is not permitted"
        );
        let addresses = self
            .upstream
            .resolve(&destination.ip().to_string(), destination.port())
            .await?
            .addresses;
        Ok(tokio::time::timeout(
            CONNECT_TIMEOUT,
            tokio::net::TcpStream::connect(addresses.as_slice()),
        )
        .await??)
    }

    fn connect_host(&self, authority: &str) -> Result<String> {
        self.check_port(443)?;
        let authority: hyper::http::uri::Authority = authority.parse()?;
        ensure!(
            authority.port_u16() == Some(443),
            "CONNECT requires port 443"
        );
        let host = canonical_egress_host(authority.host())?;
        ensure!(self.network.permits_host(&host), "host is not allowed");
        Ok(host)
    }

    async fn forward(
        &self,
        mut request: Request<Incoming>,
        sni: Option<&str>,
    ) -> Result<Response<ProxyBody>, EgressError> {
        let (destination, url) = self.destination(&request, sni)?;
        let mut headers = request.headers().clone();

        let client = self.client(&destination.host, destination.port).await?;
        let original_headers = headers.clone();
        let credentials = crate::egress_credentials::rewrite_headers(
            &self.bindings,
            &mut headers,
            &destination,
            sni.is_some(),
            &self.identity,
            &TimedResolver(self.resolver.as_deref().unwrap_or(&NoCredentials)),
        )
        .await?;
        let body = tokio::time::timeout(
            IO_TIMEOUT,
            Limited::new(request.body_mut(), self.max_request_body_bytes).collect(),
        )
        .await
        .context("reading egress request body timed out")?
        .map_err(|_| anyhow!("invalid or oversized request body"))?
        .to_bytes();
        let method = request.method().clone();
        let mut response =
            relay(method.clone(), headers, url.clone(), body.clone(), &client).await?;
        if response.status() == StatusCode::UNAUTHORIZED && !credentials.is_empty() {
            let resolver = self
                .resolver
                .as_ref()
                .context("credential resolver is unavailable")?;
            let mut refreshed = false;
            for (name, rejected) in credentials {
                refreshed |= tokio::time::timeout(
                    IO_TIMEOUT,
                    resolver.refresh(&self.identity, &name, &destination, &rejected),
                )
                .await
                .context("credential refresh timed out")??
                .is_some();
            }
            if refreshed {
                let mut headers = original_headers;
                crate::egress_credentials::rewrite_headers(
                    &self.bindings,
                    &mut headers,
                    &destination,
                    sni.is_some(),
                    &self.identity,
                    &TimedResolver(resolver.as_ref()),
                )
                .await?;
                response = relay(method, headers, url.clone(), body, &client).await?;
            }
        }
        self.follow_redirect(&destination, &url, response).await
    }

    fn destination(
        &self,
        request: &Request<Incoming>,
        sni: Option<&str>,
    ) -> Result<(EgressDestination, reqwest::Url)> {
        ensure!(
            request.method() != Method::CONNECT,
            "CONNECT is not supported on the transparent listener"
        );
        ensure!(
            request.uri().scheme().is_none() && request.uri().authority().is_none(),
            "expected origin-form request"
        );
        ensure!(
            request.headers().get_all(HOST).iter().count() == 1,
            "exactly one Host header is required"
        );
        let authority: hyper::http::uri::Authority = request
            .headers()
            .get(HOST)
            .context("missing Host")?
            .to_str()?
            .parse()?;
        let host = canonical_egress_host(authority.host())?;
        ensure!(self.network.permits_host(&host), "host is not allowed");
        let port = if sni.is_some() { 443 } else { 80 };
        self.check_port(port)?;
        ensure!(
            authority.port_u16().unwrap_or(port) == port,
            "only standard HTTP ports are supported"
        );
        if let Some(sni) = sni {
            ensure!(
                canonical_egress_host(sni)? == host,
                "TLS SNI and HTTP Host must match"
            );
        }
        let path = request
            .uri()
            .path_and_query()
            .context("missing request path")?
            .as_str()
            .to_owned();
        ensure!(
            path.starts_with('/') && !path.starts_with("//"),
            "invalid request path"
        );
        let scheme = if sni.is_some() { "https" } else { "http" };
        let url = reqwest::Url::parse(&format!("{scheme}://{host}{path}"))?;
        ensure!(
            url.host_str() == Some(host.as_str()) && url.port_or_known_default() == Some(port),
            "request changed the upstream authority"
        );
        let destination =
            crate::egress_credentials::http_destination(&url, request.method().clone())?;
        Ok((destination, url))
    }

    async fn client(&self, host: &str, port: u16) -> Result<reqwest::Client> {
        let mut upstream = self
            .upstream
            .resolve(host, port)
            .await
            .map_err(|_| anyhow!("upstream address resolution failed"))?;
        upstream.addresses.sort_unstable();
        upstream.addresses.dedup();
        let mut clients = self.clients.lock().expect("egress client pool poisoned");
        let key = (host.to_owned(), port);
        if let Some(cached) = clients.get(&key)
            && cached.addresses == upstream.addresses
        {
            return Ok(cached.client.clone());
        }
        let mut builder = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(CONNECT_TIMEOUT)
            .resolve_to_addrs(host, &upstream.addresses);
        if let Some(certificate) = upstream.root_certificate {
            builder = builder.add_root_certificate(certificate);
        }
        let client = builder.build()?;
        clients.insert(
            key,
            PooledClient {
                addresses: upstream.addresses,
                client: client.clone(),
            },
        );
        Ok(client)
    }
}

async fn relay(
    method: Method,
    mut headers: HeaderMap,
    url: reqwest::Url,
    body: Bytes,
    client: &reqwest::Client,
) -> Result<reqwest::Response> {
    headers.remove(HOST);
    headers.remove("content-length");
    client
        .request(method, url)
        .headers(headers)
        .body(body)
        .send()
        .await
        .map_err(|_| anyhow!("upstream request failed"))
}

fn proxy_response(response: reqwest::Response) -> Result<Response<ProxyBody>> {
    let status = response.status();
    let mut headers = response.headers().clone();
    strip_hop_headers(&mut headers)?;
    let stream = response
        .bytes_stream()
        .map_ok(Frame::data)
        .map_err(|_| -> ProxyError { "upstream response failed".into() });
    let mut result = Response::new(BodyExt::boxed(StreamBody::new(stream)));
    *result.status_mut() = status;
    *result.headers_mut() = headers;
    Ok(result)
}

// Conservative IPv4 unicast filter based on the IANA special-purpose registry:
// https://www.iana.org/assignments/iana-ipv4-special-registry/
// Also excludes multicast (RFC 1112). The whole 192.0.0.0/24 protocol block and
// deprecated 192.88.99.0/24 stay blocked, including their anycast exceptions.
pub(crate) fn public_ipv4(ip: IpAddr) -> bool {
    let IpAddr::V4(ip) = ip else {
        return false;
    };
    let [a, b, c, _] = ip.octets();
    !(a == 0
        || a == 10
        || a == 127
        || a >= 224
        || (a == 100 && (64..=127).contains(&b))
        || (a == 169 && b == 254)
        || (a == 172 && (16..=31).contains(&b))
        || (a == 192 && b == 168)
        || (a == 192 && b == 0 && (c == 0 || c == 2))
        || (a == 192 && b == 88 && c == 99)
        || (a == 198 && (b == 18 || b == 19))
        || (a == 198 && b == 51 && c == 100)
        || (a == 203 && b == 0 && c == 113))
}

// RFC 9110 section 7.6.1: remove connection-specific fields when forwarding.
// https://www.rfc-editor.org/rfc/rfc9110.html#section-7.6.1
// Proxy authentication is scoped to its proxy (sections 11.7.1/11.7.2); framing
// and trailers are handled by the HTTP stacks on either side of this proxy.
fn tls_configuration(hosts: Vec<String>) -> Result<(String, TlsAcceptor)> {
    let authority = TlsAuthority::new()?;
    let tls = authority.acceptor(hosts)?;
    Ok((authority.ca_pem, tls))
}

struct TlsAuthority {
    ca_pem: String,
    issuer: Issuer<'static, KeyPair>,
}

impl TlsAuthority {
    fn new() -> Result<Self> {
        let mut ca = CertificateParams::new(Vec::<String>::new())?;
        ca.distinguished_name
            .push(DnType::CommonName, "Exo sandbox egress CA");
        ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca.key_usages = vec![
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::DigitalSignature,
            KeyUsagePurpose::CrlSign,
        ];
        let key = KeyPair::generate()?;
        let certificate = ca.self_signed(&key)?;
        let issuer = Issuer::new(ca, key);
        Ok(Self {
            ca_pem: certificate.pem(),
            issuer,
        })
    }

    fn acceptor(&self, hosts: Vec<String>) -> Result<TlsAcceptor> {
        let leaf_key = KeyPair::generate()?;
        let mut leaf = CertificateParams::new(hosts)?;
        leaf.distinguished_name
            .push(DnType::CommonName, "Exo sandbox egress");
        leaf.use_authority_key_identifier_extension = true;
        let leaf = leaf.signed_by(&leaf_key, &self.issuer)?;
        let mut tls =
            ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()?
                .with_no_client_auth()
                .with_single_cert(
                    vec![leaf.der().clone()],
                    PrivatePkcs8KeyDer::from(leaf_key.serialize_der()).into(),
                )?;
        tls.alpn_protocols = vec![b"http/1.1".to_vec()];
        Ok(TlsAcceptor::from(Arc::new(tls)))
    }
}

async fn http_connection<T>(
    stream: T,
    state: impl Into<SessionState>,
    sni: Option<String>,
) -> Result<()>
where
    T: AsyncRead + AsyncWrite + Send + Unpin + 'static,
{
    let state = state.into();
    let service = service_fn(move |request| {
        let state = state.clone();
        let sni = sni.clone();
        async move {
            let result = async {
                state
                    .request(&request, sni.as_deref())?
                    .forward(request, sni.as_deref())
                    .await
            }
            .await;
            let response = match result {
                Ok(response) => response,
                Err(error) => {
                    tracing::debug!(%error, sandbox_id = %state.snapshot().identity.sandbox_id, "egress request failed");
                    let (status, message) = match error {
                        EgressError::Rejected(message) => {
                            (StatusCode::FORBIDDEN, format!("{message}\n"))
                        }
                        EgressError::Denied(_) | EgressError::Internal(_) => (
                            StatusCode::BAD_GATEWAY,
                            "egress request denied or upstream unavailable\n".into(),
                        ),
                    };
                    let body = Full::new(Bytes::from(message))
                        .map_err(|never| -> ProxyError { match never {} })
                        .boxed();
                    let mut response = Response::new(body);
                    *response.status_mut() = status;
                    response.headers_mut().insert(
                        "content-type",
                        HeaderValue::from_static("text/plain; charset=utf-8"),
                    );
                    response
                }
            };
            Ok::<_, Infallible>(response)
        }
    });
    hyper::server::conn::http1::Builder::new()
        .timer(TokioTimer::new())
        .header_read_timeout(IO_TIMEOUT)
        .max_buf_size(HTTP_BUFFER_SIZE)
        .serve_connection(TokioIo::new(stream), service)
        .await?;
    Ok(())
}

async fn https_connection<T>(
    stream: T,
    tls: TlsAcceptor,
    state: impl Into<SessionState>,
    connect_host: Option<&str>,
) -> Result<()>
where
    T: AsyncRead + AsyncWrite + Send + Unpin + 'static,
{
    let state = state.into();
    let stream = tokio::time::timeout(IO_TIMEOUT, tls.accept(stream)).await??;
    let sni = stream
        .get_ref()
        .1
        .server_name()
        .context("TLS SNI is required")?
        .to_owned();
    if let Some(host) = connect_host {
        ensure!(
            canonical_egress_host(&sni)? == host,
            "CONNECT host and TLS SNI must match"
        );
    }
    state.destination(&sni, 443, false)?;
    http_connection(stream, state, Some(sni)).await
}

async fn transparent_https_connection<T>(
    mut stream: T,
    tls: Option<TlsAcceptor>,
    state: impl Into<SessionState>,
) -> Result<()>
where
    T: AsyncRead + AsyncWrite + Send + Unpin + 'static,
{
    let session = state.into();
    let state = session.snapshot();
    state.check_port(443)?;
    if !state.network.unrestricted()
        && let Some(tls) = &tls
    {
        return https_connection(stream, tls.clone(), session, None).await;
    }
    let (host, prefix) = tokio::time::timeout(IO_TIMEOUT, async {
        let mut acceptor = rustls::server::Acceptor::default();
        let mut prefix = Vec::new();
        let mut buffer = [0; 4096];
        loop {
            let count = stream.read(&mut buffer).await?;
            ensure!(count > 0, "TLS connection closed before ClientHello");
            ensure!(
                prefix.len() + count <= 64 * 1024,
                "TLS ClientHello too large"
            );
            prefix.extend_from_slice(&buffer[..count]);
            acceptor.read_tls(&mut &buffer[..count])?;
            if let Some(accepted) = acceptor.accept().map_err(|(error, _alert)| error)? {
                let host = canonical_egress_host(
                    accepted
                        .client_hello()
                        .server_name()
                        .context("TLS SNI is required")?,
                )?;
                return Ok::<_, anyhow::Error>((host, prefix));
            }
        }
    })
    .await??;
    let (read, write) = tokio::io::split(stream);
    let mut stream = tokio::io::join(std::io::Cursor::new(prefix).chain(read), write);
    let state = session.destination(&host, 443, false)?;
    ensure!(state.network.permits_host(&host), "host is not allowed");
    if state.hosts.contains(&host)
        && let Some(tls) = tls
    {
        return https_connection(stream, tls, session, Some(&host)).await;
    }
    let mut upstream = if let Some(proxy) = &state.external {
        proxy.connect(&host).await?
    } else {
        let addresses = state.upstream.resolve(&host, 443).await?.addresses;
        Box::pin(
            tokio::time::timeout(
                CONNECT_TIMEOUT,
                tokio::net::TcpStream::connect(addresses.as_slice()),
            )
            .await??,
        )
    };
    tokio::io::copy_bidirectional(&mut stream, &mut upstream).await?;
    Ok(())
}

pub(crate) enum Connection {
    Http(crate::BoxSandboxTcpStream),
    Https(crate::BoxSandboxTcpStream),
    Tcp {
        destination: SocketAddr,
        downstream: crate::BoxSandboxTcpStream,
        upstream: tokio::net::TcpStream,
    },
}

impl ProxyConnection {
    pub(crate) fn start<A, F, C>(
        state: State,
        cancel: CancellationToken,
        accept: A,
        close: impl FnOnce() + Send + 'static,
    ) -> Result<Self>
    where
        A: Fn(Arc<State>) -> F + Send + 'static,
        F: std::future::Future<Output = Result<C>> + Send + 'static,
        C: std::future::Future<Output = Result<Connection>> + Send + 'static,
    {
        let environment = state.environment();
        let authority = state
            .external
            .is_none()
            .then(TlsAuthority::new)
            .transpose()?;
        let ca_pem = match (&state.external, &authority) {
            (Some(external), _) => external.ca_pem.clone(),
            (_, Some(authority)) => authority.ca_pem.clone(),
            _ => unreachable!(),
        };
        let tls = authority
            .as_ref()
            .map(|authority| authority.acceptor(state.hosts.iter().cloned().collect()))
            .transpose()?;
        let (updates, receiver) = mpsc::channel(1);
        let shutdown = cancel.clone();
        let task = tokio::spawn(async move {
            serve(accept, tls, authority, Arc::new(state), receiver, shutdown).await;
            close();
        });
        Ok(Self {
            ca_pem,
            environment: Mutex::new(environment),
            updates,
            cancel,
            task,
        })
    }

    async fn update(&self, mut state: State) -> Result<()> {
        if let Some(external) = &state.external {
            ensure!(
                external.ca_pem == self.ca_pem,
                "an allocation must keep its trusted proxy CA"
            );
        }
        if state.external.is_none() {
            let environment = self
                .environment
                .lock()
                .expect("egress environment poisoned");
            for binding in &mut state.bindings {
                if let Some(placeholder) = environment.get(&binding.config.environment_variable) {
                    binding.placeholder.clone_from(placeholder);
                }
            }
        }
        let environment = state.environment();
        let (response, result) = oneshot::channel();
        self.updates
            .send((state, response))
            .await
            .context("egress proxy is closed")?;
        result.await.context("egress update did not complete")??;
        *self
            .environment
            .lock()
            .expect("egress environment poisoned") = environment;
        Ok(())
    }

    fn close(&self) {
        self.cancel.cancel();
    }
}

impl Drop for ProxyConnection {
    fn drop(&mut self) {
        self.close();
        self.task.abort();
    }
}

async fn serve<A, F, C>(
    accept: A,
    mut tls: Option<TlsAcceptor>,
    authority: Option<TlsAuthority>,
    state: Arc<State>,
    mut updates: mpsc::Receiver<(State, oneshot::Sender<Result<()>>)>,
    cancel: CancellationToken,
) where
    A: Fn(Arc<State>) -> F,
    F: std::future::Future<Output = Result<C>>,
    C: std::future::Future<Output = Result<Connection>> + Send + 'static,
{
    let mut tasks = JoinSet::new();
    let connections = Arc::new(Mutex::new(Connections {
        state,
        active: HashMap::new(),
    }));
    let permits = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            Some(result) = tasks.join_next_with_id(), if !tasks.is_empty() => {
                let id = match &result { Ok((id, _)) => *id, Err(error) => error.id() };
                connections.lock().expect("egress connections poisoned").active.remove(&id);
                if !matches!(result, Ok((_, Ok(())))) {
                    tracing::debug!("egress connection closed with an error");
                }
            }
            Some((next, response)) = updates.recv() => {
                let next_tls = authority.as_ref().map(|authority| authority.acceptor(next.hosts.iter().cloned().collect())).transpose();
                match next_tls {
                    Ok(next_tls) => {
                        let mut cancelled = {
                            let mut connections = connections.lock().expect("egress connections poisoned");
                            let mut cancelled = HashSet::new();
                            let same_proxy = match (&connections.state.external, &next.external) {
                                (Some(old), Some(new)) => old.url == new.url
                                    && old.username == new.username
                                    && old.password == new.password
                                    && old.ca_pem == new.ca_pem,
                                (None, None) => true,
                                _ => false,
                            };
                            let same_identity = connections.state.identity == next.identity;
                            let same_resolver = match (&connections.state.resolver, &next.resolver) {
                                (Some(old), Some(new)) => Arc::ptr_eq(old, new),
                                (None, None) => true,
                                _ => false,
                            };
                            for (id, connection) in &connections.active {
                                if !same_identity || !same_proxy || connection.destinations.iter().any(|destination| !connections.state.preserves(&next, destination, same_resolver)) {
                                    connection.abort.abort();
                                    cancelled.insert(*id);
                                }
                            }
                            connections.state = Arc::new(next);
                            cancelled
                        };
                        while !cancelled.is_empty() {
                            let Some(result) = tasks.join_next_with_id().await else { break; };
                            let id = match result { Ok((id, _)) => id, Err(error) => error.id() };
                            connections.lock().expect("egress connections poisoned").active.remove(&id);
                            cancelled.remove(&id);
                        }
                        tls = next_tls;
                        let _ = response.send(Ok(()));
                    }
                    Err(error) => {
                        let _ = response.send(Err(error));
                        break;
                    }
                }
            }
            incoming = {
                let state = connections.lock().expect("egress connections poisoned").state.clone();
                accept(state)
            } => {
                let Ok(connection) = incoming else { break; };
                let Ok(permit) = permits.clone().try_acquire_owned() else { continue; };
                let tls = tls.clone();
                let mut active = connections.lock().expect("egress connections poisoned");
                let shared = connections.clone();
                let task = tasks.spawn(async move {
                    let state = SessionState::Live(ConnectionState { connections: shared, id: tokio::task::id() });
                    let _permit = permit;
                    match connection.await? {
                        Connection::Http(stream) => http_connection(stream, state, None).await,
                        Connection::Https(stream) => transparent_https_connection(stream, tls, state).await,
                        Connection::Tcp { mut downstream, mut upstream, destination } => {
                            state.destination(&destination.ip().to_string(), destination.port(), true)?;
                            tokio::io::copy_bidirectional(&mut downstream, &mut upstream).await?;
                            Ok(())
                        }
                    }
                });
                active.active.insert(task.id(), ActiveConnection { destinations: HashSet::new(), abort: task });
            }
        }
    }
    cancel.cancel();
    tasks.shutdown().await;
}

#[cfg(test)]
mod tests;

pub(super) const PREPARE_TRUST: &str = r#"set -eu
umask 077
bundle=$(mktemp "$EXO_EGRESS_CA_PATH.XXXXXX")
trap 'rm -f "$bundle"' EXIT
for system in /etc/ssl/certs/ca-certificates.crt /etc/ssl/cert.pem /etc/pki/tls/certs/ca-bundle.crt; do
  if [ -f "$system" ]; then
    cat "$system" > "$bundle"
    break
  fi
done
printf '\n%s\n' "$EXO_EGRESS_CA_PEM" >> "$bundle"
mv -f "$bundle" "$EXO_EGRESS_CA_PATH"
"#;

pub(crate) const TRUST_VARIABLES: [&str; 6] = [
    "SSL_CERT_FILE",
    "CODEX_CA_CERTIFICATE",
    "REQUESTS_CA_BUNDLE",
    "NODE_EXTRA_CA_CERTS",
    "CURL_CA_BUNDLE",
    "GIT_SSL_CAINFO",
];

/// Environment that routes a guest's HTTP clients through `proxy_url` and makes
/// them trust the CA bundle installed at `ca_path`.
pub(crate) fn guest_proxy_environment(proxy_url: &str, ca_path: &str) -> HashMap<String, String> {
    let mut environment = HashMap::with_capacity(TRUST_VARIABLES.len() + 7);
    for variable in ["HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy"] {
        environment.insert(variable.into(), proxy_url.into());
    }
    for variable in ["NO_PROXY", "no_proxy"] {
        environment.insert(variable.into(), "localhost,127.0.0.1,::1".into());
    }
    environment.insert("NODE_USE_ENV_PROXY".into(), "1".into());
    for variable in TRUST_VARIABLES {
        environment.insert(variable.into(), ca_path.into());
    }
    environment
}

struct TimedResolver<'a>(&'a dyn EgressCredentialResolver);
#[async_trait]
impl EgressCredentialResolver for TimedResolver<'_> {
    async fn resolve(
        &self,
        identity: &EgressIdentity,
        name: &str,
        destination: &EgressDestination,
    ) -> Result<String, EgressError> {
        tokio::time::timeout(IO_TIMEOUT, self.0.resolve(identity, name, destination))
            .await
            .context("credential resolution timed out")?
    }
}

struct NoCredentials;
#[async_trait]
impl EgressCredentialResolver for NoCredentials {
    async fn resolve(
        &self,
        _identity: &EgressIdentity,
        _name: &str,
        _destination: &EgressDestination,
    ) -> Result<String, EgressError> {
        Err(anyhow!("credential resolver is unavailable").into())
    }
}
