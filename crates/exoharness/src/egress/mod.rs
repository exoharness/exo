//! HTTP/TLS handling and credential substitution outside the sandbox.
//!
//! `EgressTransport` supplies connections from the sandbox network. This module
//! checks destinations, resolves credentials, and forwards requests. `sandbox`
//! coordinates proxy setup with Firecracker/Lima acquisition; `transport`
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
use hyper::header::{CONNECTION, HOST, HeaderMap, HeaderName, HeaderValue};
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::{TokioIo, TokioTimer};
use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, Issuer, KeyPair, KeyUsagePurpose};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls::{self, ServerConfig, pki_types::PrivatePkcs8KeyDer};
use tokio_util::sync::CancellationToken;

use crate::types::{canonical_egress_host, canonical_egress_hosts};

use crate::{
    CredentialNetworkPolicy, EgressCredentialBinding, EgressPolicy, SandboxEgressProxy,
    SandboxNetworkPolicy,
};

mod explicit;
pub use explicit::{ExplicitProxy, ProxyAuthorizer, ProxySession, serve_connect_proxy};
mod transport;
pub use transport::{EgressTransport, LocalEgressTransport};
#[cfg(feature = "firecracker")]
mod sandbox;
#[cfg(feature = "firecracker")]
pub(crate) use sandbox::{EgressRuntime, SandboxEgress};

const PLACEHOLDER_PREFIX: &str = "exo_egress_";
const IO_TIMEOUT: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_REQUEST_BODY: usize = 8 * 1024 * 1024;
const HTTP_BUFFER_SIZE: usize = 32 * 1024;
const MAX_CONNECTIONS: usize = 128;

pub(crate) struct ResolvedUpstream {
    addresses: Vec<SocketAddr>,
    root_certificate: Option<reqwest::Certificate>,
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

#[derive(Debug, Clone)]
pub struct EgressIdentity {
    pub sandbox_id: String,
    pub scope: Option<crate::SandboxScope>,
}

#[derive(Debug)]
pub struct EgressDestination {
    pub host: String,
    pub port: u16,
    pub method: Method,
    pub path: String,
}

/// Looks up a binding for this sandbox's agent/thread on every use. The local
/// implementation reads Exo's encrypted store; a hosted implementation can use
/// its own vault and authorization. Only the proxy receives the returned value.
#[async_trait]
pub trait EgressCredentialResolver: Send + Sync {
    async fn resolve(
        &self,
        identity: &EgressIdentity,
        binding_name: &str,
        destination: &EgressDestination,
    ) -> Result<String>;

    async fn resolve_with_request(
        &self,
        identity: &EgressIdentity,
        binding_name: &str,
        destination: &EgressDestination,
        _body: &[u8],
    ) -> Result<String> {
        self.resolve(identity, binding_name, destination).await
    }
}

// Owns one sandbox's TLS server, placeholders, and active proxy connections.
// It does not create VMs or decide where credentials are stored.
pub struct EgressProxy {
    endpoints: SandboxEgressProxy,
    transport: Arc<dyn EgressTransport>,
    ca_pem: String,
    environment: HashMap<String, String>,
    cancel: CancellationToken,
    task: tokio::task::JoinHandle<()>,
}

struct Binding {
    config: EgressCredentialBinding,
    hosts: HashSet<String>,
    placeholder: String,
}

impl Binding {
    fn permits_header(&self, host: &str) -> bool {
        self.config.injection_location.header && self.hosts.contains(host)
    }
}

struct PooledClient {
    addresses: Vec<SocketAddr>,
    client: reqwest::Client,
}

// Request handling shared by all connections to one sandbox's proxy. Clients
// are pooled by destination and replaced whenever its resolved addresses change.
struct State {
    clients: Mutex<HashMap<(String, u16), PooledClient>>,
    hosts: HashSet<String>,
    unrestricted: bool,
    bindings: Vec<Binding>,
    identity: EgressIdentity,
    resolver: Option<Arc<dyn EgressCredentialResolver>>,
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
        ensure!(
            !state.unrestricted,
            "transparent egress proxy requires limited networking"
        );
        let endpoints = transport.endpoints();
        endpoints.validate()?;
        let (ca_pem, tls) = tls_configuration(state.hosts.iter().cloned().collect())?;
        let environment = state
            .bindings
            .iter()
            .map(|b| (b.config.environment_variable.clone(), b.placeholder.clone()))
            .collect();
        let state = Arc::new(state);
        let task = tokio::spawn(serve(transport.clone(), tls, state, cancel.clone()));
        Ok(Self {
            endpoints,
            transport,
            ca_pem,
            environment,
            cancel,
            task,
        })
    }

    pub async fn bind_source(&self, source_ip: Ipv4Addr) -> Result<()> {
        self.transport.bind_source(source_ip).await
    }

    pub fn endpoints(&self) -> SandboxEgressProxy {
        self.endpoints
    }

    pub fn ca_pem(&self) -> &str {
        &self.ca_pem
    }

    pub fn environment(&self) -> &HashMap<String, String> {
        &self.environment
    }

    pub fn close(&self) {
        self.cancel.cancel();
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
        self.task.abort();
    }
}

impl State {
    fn new(
        identity: EgressIdentity,
        policy: EgressPolicy,
        resolver: Option<Arc<dyn EgressCredentialResolver>>,
        upstream: Arc<dyn UpstreamResolver>,
    ) -> Result<Self> {
        Self::new_with_placeholders(identity, policy, resolver, upstream, None)
    }

    fn new_with_placeholders(
        identity: EgressIdentity,
        policy: EgressPolicy,
        resolver: Option<Arc<dyn EgressCredentialResolver>>,
        upstream: Arc<dyn UpstreamResolver>,
        placeholders: Option<&HashMap<String, String>>,
    ) -> Result<Self> {
        ensure!(
            !identity.sandbox_id.is_empty(),
            "egress identity is required"
        );
        let unrestricted = policy.networking == SandboxNetworkPolicy::Unrestricted;
        let hosts = match &policy.networking {
            SandboxNetworkPolicy::Limited { allowed_hosts } => {
                canonical_egress_hosts(allowed_hosts)?
            }
            SandboxNetworkPolicy::Unrestricted => {
                policy
                    .credentials
                    .iter()
                    .try_fold(HashSet::new(), |mut hosts, binding| {
                        let CredentialNetworkPolicy::Limited { allowed_hosts } =
                            &binding.networking;
                        hosts.extend(canonical_egress_hosts(allowed_hosts)?);
                        Ok::<_, anyhow::Error>(hosts)
                    })?
            }
            SandboxNetworkPolicy::Disabled => anyhow::bail!("credential proxy requires networking"),
        };
        ensure!(
            policy.credentials.is_empty() || resolver.is_some(),
            "credential substitution requires an egress credential resolver"
        );
        let mut variables = HashSet::new();
        let mut values = HashSet::new();
        if let Some(placeholders) = placeholders {
            ensure!(
                placeholders.len() == policy.credentials.len(),
                "placeholder map must match credential bindings"
            );
        }
        let mut bindings = Vec::new();
        for config in policy.credentials {
            let CredentialNetworkPolicy::Limited { allowed_hosts } = &config.networking;
            let hosts = canonical_egress_hosts(allowed_hosts)?;
            ensure!(
                !config.name.is_empty(),
                "credential binding name is required"
            );
            ensure!(
                !config.environment_variable.is_empty()
                    && config
                        .environment_variable
                        .bytes()
                        .enumerate()
                        .all(|(i, c)| c == b'_'
                            || c.is_ascii_alphabetic()
                            || (i > 0 && c.is_ascii_digit())),
                "invalid credential environment variable"
            );
            ensure!(
                variables.insert(config.environment_variable.clone()),
                "duplicate credential environment variable"
            );
            let placeholder = if let Some(placeholders) = placeholders {
                let value = placeholders
                    .get(&config.environment_variable)
                    .context("missing credential placeholder")?;
                ensure!(
                    value.len() == PLACEHOLDER_PREFIX.len() + 32
                        && value.starts_with(PLACEHOLDER_PREFIX)
                        && value[PLACEHOLDER_PREFIX.len()..]
                            .bytes()
                            .all(|byte| byte.is_ascii_hexdigit())
                        && values.insert(value.clone()),
                    "invalid or duplicate credential placeholder"
                );
                value.clone()
            } else {
                format!("{PLACEHOLDER_PREFIX}{}", uuid::Uuid::new_v4().simple())
            };
            bindings.push(Binding {
                config,
                hosts,
                placeholder,
            });
        }
        Ok(Self {
            clients: Mutex::new(HashMap::new()),
            hosts,
            unrestricted,
            bindings,
            identity,
            resolver,
            upstream,
        })
    }

    fn connect_host(&self, authority: &str) -> Result<String> {
        let authority: hyper::http::uri::Authority = authority.parse()?;
        ensure!(
            authority.port_u16() == Some(443),
            "CONNECT requires port 443"
        );
        let host = canonical_egress_host(authority.host())?;
        ensure!(
            self.unrestricted || self.hosts.contains(&host),
            "host is not allowed"
        );
        Ok(host)
    }

    async fn forward(
        &self,
        mut request: Request<Incoming>,
        sni: Option<&str>,
    ) -> Result<Response<ProxyBody>> {
        let (destination, url) = self.destination(&request, sni)?;
        let mut headers = request.headers().clone();
        self.validate_credentials(&headers, &destination.host, sni.is_some())?;
        strip_hop_headers(&mut headers)?;
        let client = self.client(&destination.host, destination.port).await?;
        let body = tokio::time::timeout(
            IO_TIMEOUT,
            Limited::new(request.body_mut(), MAX_REQUEST_BODY).collect(),
        )
        .await?
        .map_err(|_| anyhow!("invalid or oversized request body"))?
        .to_bytes();
        self.substitute_credentials(&mut headers, &destination, &body)
            .await?;
        relay(request.method().clone(), body, headers, url, client).await
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
            !request.headers().contains_key("upgrade"),
            "protocol upgrades are not supported"
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
        ensure!(
            self.unrestricted || self.hosts.contains(&host),
            "host is not allowed"
        );
        let port = if sni.is_some() { 443 } else { 80 };
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
        let path = match url.query() {
            Some(query) => format!("{}?{query}", url.path()),
            None => url.path().to_owned(),
        };
        let destination = EgressDestination {
            host,
            port,
            method: request.method().clone(),
            path,
        };
        Ok((destination, url))
    }

    fn validate_credentials(&self, headers: &HeaderMap, host: &str, tls: bool) -> Result<()> {
        for (header, value) in headers {
            let basic = basic_credential_placeholder(header, value)?;
            let value = basic
                .as_deref()
                .map(str::as_bytes)
                .unwrap_or(value.as_bytes());
            if !contains_placeholder(value) {
                continue;
            }
            ensure!(tls, "credential substitution requires HTTPS");
            ensure!(
                !hop_header(header) && header != HOST && header != "content-length",
                "credential cannot rewrite HTTP routing or framing"
            );
            ensure!(
                headers.get_all(header).iter().count() == 1,
                "duplicate credential header"
            );
            let mut unresolved = std::str::from_utf8(value)?.to_owned();
            for binding in &self.bindings {
                if binding.permits_header(host) {
                    unresolved = unresolved.replace(&binding.placeholder, "");
                }
            }
            ensure!(
                !unresolved.contains(PLACEHOLDER_PREFIX),
                "credential placeholder does not match this request"
            );
        }
        Ok(())
    }

    async fn substitute_credentials(
        &self,
        headers: &mut HeaderMap,
        destination: &EgressDestination,
        body: &[u8],
    ) -> Result<()> {
        for (header, header_value) in headers.iter_mut() {
            let basic = basic_credential_placeholder(header, header_value)?;
            if basic.is_none() && !contains_placeholder(header_value.as_bytes()) {
                continue;
            }
            let encode_basic = basic.is_some();
            let mut replacement = match basic {
                Some(value) => value,
                None => header_value.to_str()?.to_owned(),
            };
            for binding in &self.bindings {
                if !binding.permits_header(&destination.host)
                    || !replacement.contains(&binding.placeholder)
                {
                    continue;
                }
                let value = tokio::time::timeout(
                    IO_TIMEOUT,
                    self.resolver
                        .as_ref()
                        .context("credential resolver is unavailable")?
                        .resolve_with_request(
                            &self.identity,
                            &binding.config.name,
                            destination,
                            body,
                        ),
                )
                .await?
                .map_err(|_| anyhow!("credential is unavailable or not authorized"))?;
                replacement = replacement.replace(&binding.placeholder, &value);
            }
            if encode_basic {
                replacement = format!(
                    "Basic {}",
                    base64::engine::general_purpose::STANDARD.encode(replacement)
                );
            }
            let mut value = HeaderValue::from_str(&replacement)
                .map_err(|_| anyhow!("credential cannot be used in an HTTP header"))?;
            value.set_sensitive(true);
            *header_value = value;
        }
        Ok(())
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

fn basic_credential_placeholder(
    header: &HeaderName,
    value: &HeaderValue,
) -> Result<Option<String>> {
    if header == hyper::header::AUTHORIZATION
        && let Ok(value) = value.to_str()
        && let Some((scheme, encoded)) = value.split_once(' ')
        && scheme.eq_ignore_ascii_case("basic")
        && let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(encoded)
        && contains_placeholder(&decoded)
    {
        return Ok(Some(
            String::from_utf8(decoded).context("invalid Basic credential")?,
        ));
    }
    Ok(None)
}

fn contains_placeholder(value: &[u8]) -> bool {
    value
        .windows(PLACEHOLDER_PREFIX.len())
        .any(|s| s == PLACEHOLDER_PREFIX.as_bytes())
}

async fn relay(
    method: Method,
    body: Bytes,
    mut headers: HeaderMap,
    url: reqwest::Url,
    client: reqwest::Client,
) -> Result<Response<ProxyBody>> {
    headers.remove(HOST);
    headers.remove("content-length");
    let response = client
        .request(method, url)
        .headers(headers)
        .body(body)
        .send()
        .await
        .map_err(|_| anyhow!("upstream request failed"))?;
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
fn public_ipv4(ip: IpAddr) -> bool {
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
fn hop_header(name: &HeaderName) -> bool {
    // HeaderName::as_str() always returns lowercase; no normalization is needed.
    // https://docs.rs/http/latest/http/header/struct.HeaderName.html#method.as_str
    matches!(
        name.as_str(),
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "proxy-connection"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
    )
}

fn strip_hop_headers(headers: &mut HeaderMap) -> Result<()> {
    let mut remove = Vec::new();
    // The same rule also removes custom fields named by Connection.
    for value in headers.get_all(CONNECTION) {
        remove.extend(
            value
                .to_str()?
                .split(',')
                .filter_map(|name| HeaderName::from_bytes(name.trim().as_bytes()).ok()),
        );
    }
    remove.extend(headers.keys().filter(|name| hop_header(name)).cloned());
    for name in remove {
        headers.remove(name);
    }
    Ok(())
}

fn tls_configuration(hosts: Vec<String>) -> Result<(String, TlsAcceptor)> {
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
    let leaf_key = KeyPair::generate()?;
    let mut leaf = CertificateParams::new(hosts)?;
    leaf.distinguished_name
        .push(DnType::CommonName, "Exo sandbox egress");
    leaf.use_authority_key_identifier_extension = true;
    let leaf = leaf.signed_by(&leaf_key, &issuer)?;
    let mut tls =
        ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()?
            .with_no_client_auth()
            .with_single_cert(
                vec![leaf.der().clone()],
                PrivatePkcs8KeyDer::from(leaf_key.serialize_der()).into(),
            )?;
    tls.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok((certificate.pem(), TlsAcceptor::from(Arc::new(tls))))
}

async fn http_connection<T>(stream: T, state: Arc<State>, sni: Option<String>) -> Result<()>
where
    T: AsyncRead + AsyncWrite + Send + Unpin + 'static,
{
    let service = service_fn(move |request| {
        let state = state.clone();
        let sni = sni.clone();
        async move {
            let response = match state.forward(request, sni.as_deref()).await {
                Ok(response) => response,
                Err(error) => {
                    tracing::debug!(%error, sandbox_id = %state.identity.sandbox_id, "egress request failed");
                    let body = Full::new(Bytes::from_static(
                        b"egress request denied or upstream unavailable\n",
                    ))
                    .map_err(|never| -> ProxyError { match never {} })
                    .boxed();
                    let mut response = Response::new(body);
                    *response.status_mut() = StatusCode::BAD_GATEWAY;
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
    state: Arc<State>,
    connect_host: Option<&str>,
) -> Result<()>
where
    T: AsyncRead + AsyncWrite + Send + Unpin + 'static,
{
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
    http_connection(stream, state, Some(sni)).await
}

async fn serve(
    transport: Arc<dyn EgressTransport>,
    tls: TlsAcceptor,
    state: Arc<State>,
    cancel: CancellationToken,
) {
    let mut tasks = JoinSet::new();
    let permits = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            Some(result) = tasks.join_next(), if !tasks.is_empty() => {
                if !matches!(result, Ok(Ok(()))) {
                    tracing::debug!("egress connection closed with an error");
                }
            }
            incoming = transport.accept(false) => {
                let Ok(stream) = incoming else { break; };
                let Ok(permit) = permits.clone().try_acquire_owned() else { continue; };
                let state = state.clone();
                tasks.spawn(async move {
                    let _permit = permit;
                    http_connection(stream, state, None).await
                });
            }
            incoming = transport.accept(true) => {
                let Ok(stream) = incoming else { break; };
                let Ok(permit) = permits.clone().try_acquire_owned() else { continue; };
                let state = state.clone();
                let tls = tls.clone();
                tasks.spawn(async move {
                    let _permit = permit;
                    https_connection(stream, tls, state, None).await
                });
            }
        }
    }
    cancel.cancel();
    transport.close();
    tasks.shutdown().await;
}

#[cfg(test)]
mod tests;
