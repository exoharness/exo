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
use bytes::Bytes;
use futures::TryStreamExt;
use http_body_util::{BodyExt, Full, Limited, StreamBody, combinators::BoxBody};
use hyper::body::{Body, Frame, Incoming};
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

mod transport;
pub use transport::{EgressTransport, LocalEgressTransport};
mod sandbox;
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

pub type EgressResponseBody = BoxBody<Bytes, Box<dyn std::error::Error + Send + Sync>>;
type ProxyError = Box<dyn std::error::Error + Send + Sync>;

#[derive(Debug, Clone)]
pub struct EgressIdentity {
    pub sandbox_id: String,
    pub scope: Option<crate::SandboxScope>,
}

#[derive(Debug)]
pub struct EgressDestination {
    pub scheme: String,
    pub host: String,
    pub port: u16,
    pub method: Method,
    pub path: String,
}

/// A URL prefix and method mapping. A matching suffix and query are carried
/// from the inbound URL to the upstream URL. An empty method list permits every
/// method except CONNECT, which the engine always rejects.
#[derive(Debug, Clone)]
pub struct EgressRule {
    pub inbound_prefix: String,
    pub upstream_prefix: String,
    pub methods: Vec<Method>,
}

/// A credential placeholder supplied with a request policy. The binding's
/// destination allowlist is checked after URL rewriting.
#[derive(Debug, Clone)]
pub struct EgressRequestCredential {
    pub binding: EgressCredentialBinding,
    pub placeholder: String,
}

/// Per-request rules and placeholders. The caller supplies this alongside the
/// sandbox identity; Exo does not retain an HTTP caller's sandbox session.
#[derive(Debug, Clone)]
pub struct EgressRequestPolicy {
    pub rules: Vec<EgressRule>,
    pub credentials: Vec<EgressRequestCredential>,
}

/// Looks up a binding for this sandbox's agent/thread on every use. The local
/// implementation reads Exo's encrypted store; a hosted implementation can use
/// its own vault and authorization. Only the proxy receives the returned value.
#[async_trait]
pub trait EgressCredentialResolver: Send + Sync {
    /// Authorize the normalized upstream destination on every request, even
    /// when no credential placeholder is present.
    async fn authorize(
        &self,
        identity: &EgressIdentity,
        destination: &EgressDestination,
        credential_bindings: &[String],
    ) -> Result<()>;

    async fn resolve(
        &self,
        identity: &EgressIdentity,
        binding_name: &str,
        destination: &EgressDestination,
    ) -> Result<String>;
}

// Owns one sandbox's TLS server, placeholders, and active proxy connections.
// It does not create VMs or decide where credentials are stored.
struct EgressProxy {
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

struct CompiledRule {
    inbound: reqwest::Url,
    upstream: reqwest::Url,
    methods: Vec<Method>,
}

impl CompiledRule {
    fn new(rule: EgressRule) -> Result<Self> {
        let inbound = parse_egress_url(&rule.inbound_prefix)?;
        let upstream = parse_egress_url(&rule.upstream_prefix)?;
        ensure!(
            inbound.query().is_none() && upstream.query().is_none(),
            "egress rule prefixes cannot contain a query"
        );
        Ok(Self {
            inbound,
            upstream,
            methods: rule.methods,
        })
    }

    fn matches(&self, url: &reqwest::Url, method: &Method) -> bool {
        if url.scheme() != self.inbound.scheme()
            || url.host_str() != self.inbound.host_str()
            || url.port_or_known_default() != self.inbound.port_or_known_default()
            || (!self.methods.is_empty() && !self.methods.contains(method))
        {
            return false;
        }
        let Some(suffix) = url.path().strip_prefix(self.inbound.path()) else {
            return false;
        };
        suffix.is_empty() || suffix.starts_with('/') || self.inbound.path().ends_with('/')
    }
}

fn parse_egress_url(value: &str) -> Result<reqwest::Url> {
    let url = reqwest::Url::parse(value)?;
    ensure!(
        matches!(url.scheme(), "http" | "https"),
        "only HTTP and HTTPS are supported"
    );
    ensure!(
        url.username().is_empty() && url.password().is_none() && url.fragment().is_none(),
        "egress URLs cannot contain credentials or fragments"
    );
    let host = url.host_str().context("egress URL requires a host")?;
    ensure!(
        canonical_egress_host(host)? == host,
        "invalid egress URL host"
    );
    let port = if url.scheme() == "https" { 443 } else { 80 };
    ensure!(
        url.port_or_known_default() == Some(port),
        "only standard HTTP ports are supported"
    );
    ensure!(!url.path().starts_with("//"), "invalid egress URL path");
    Ok(url)
}

fn rule_destination(
    rules: &[CompiledRule],
    inbound_url: &str,
    method: &Method,
) -> Result<(EgressDestination, reqwest::Url)> {
    let inbound = parse_egress_url(inbound_url)?;
    let rule = rules
        .iter()
        .find(|rule| rule.matches(&inbound, method))
        .context("URL or method is not allowed")?;
    let suffix = inbound
        .path()
        .strip_prefix(rule.inbound.path())
        .expect("matched prefix");
    let query = inbound
        .query()
        .map(|query| format!("?{query}"))
        .unwrap_or_default();
    let url = parse_egress_url(&format!("{}{}{}", rule.upstream, suffix, query))?;
    ensure!(
        url.scheme() == rule.upstream.scheme()
            && url.host_str() == rule.upstream.host_str()
            && url.port_or_known_default() == rule.upstream.port_or_known_default(),
        "rule changed the upstream authority"
    );
    Ok((egress_destination(&url, method)?, url))
}

fn egress_destination(url: &reqwest::Url, method: &Method) -> Result<EgressDestination> {
    let path = match url.query() {
        Some(query) => format!("{}?{query}", url.path()),
        None => url.path().to_owned(),
    };
    Ok(EgressDestination {
        scheme: url.scheme().to_owned(),
        host: url.host_str().context("missing upstream host")?.to_owned(),
        port: url
            .port_or_known_default()
            .context("missing upstream port")?,
        method: method.clone(),
        path,
    })
}

// Firecracker reuses this state per sandbox; the HTTP API creates it per request.
struct State {
    clients: Mutex<HashMap<(String, u16), PooledClient>>,
    hosts: HashSet<String>,
    bindings: Vec<Binding>,
    identity: EgressIdentity,
    resolver: Option<Arc<dyn EgressCredentialResolver>>,
    upstream: Arc<dyn UpstreamResolver>,
}

impl EgressProxy {
    async fn start_with_transport(
        transport: Arc<dyn EgressTransport>,
        state: State,
        cancel: CancellationToken,
    ) -> Result<Self> {
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

    async fn bind_source(&self, source_ip: Ipv4Addr) -> Result<()> {
        self.transport.bind_source(source_ip).await
    }

    fn endpoints(&self) -> SandboxEgressProxy {
        self.endpoints
    }

    fn ca_pem(&self) -> &str {
        &self.ca_pem
    }

    fn environment(&self) -> &HashMap<String, String> {
        &self.environment
    }

    fn close(&self) {
        self.cancel.cancel();
        self.transport.close();
    }
}

impl Drop for EgressProxy {
    fn drop(&mut self) {
        self.close();
        self.task.abort();
    }
}

/// Reusable request engine with no sandbox session or listener state.
pub struct EgressEngine {
    upstream: Arc<dyn UpstreamResolver>,
}

impl EgressEngine {
    pub fn new() -> Self {
        Self {
            upstream: Arc::new(PublicUpstreamResolver),
        }
    }

    /// Forward one authenticated sandbox request. Rules map the request URI to
    /// the upstream. Identity and policy are supplied for every request. The
    /// named capability header is removed before the upstream request is built.
    pub async fn forward_http_request<B>(
        &self,
        identity: EgressIdentity,
        policy: EgressRequestPolicy,
        resolver: Arc<dyn EgressCredentialResolver>,
        capability_header: HeaderName,
        request: Request<B>,
    ) -> Result<Response<EgressResponseBody>>
    where
        B: Body<Data = Bytes> + Send + 'static,
        B::Error: Into<ProxyError> + Send + Sync + 'static,
    {
        ensure!(
            !hop_header(&capability_header)
                && capability_header != HOST
                && capability_header != "content-length",
            "invalid capability header name"
        );
        ensure!(!policy.rules.is_empty(), "egress rules are required");
        let rules = policy
            .rules
            .into_iter()
            .map(CompiledRule::new)
            .collect::<Result<Vec<_>>>()?;
        let (destination, url) =
            rule_destination(&rules, &request.uri().to_string(), request.method())?;
        let state = State::new_request(
            identity,
            policy.credentials,
            Some(resolver),
            self.upstream.clone(),
        )?;
        Self::forward(&state, request, destination, url, Some(&capability_header)).await
    }

    async fn forward<B>(
        state: &State,
        request: Request<B>,
        destination: EgressDestination,
        url: reqwest::Url,
        capability_header: Option<&HeaderName>,
    ) -> Result<Response<EgressResponseBody>>
    where
        B: Body<Data = Bytes> + Send + 'static,
        B::Error: Into<ProxyError> + Send + Sync + 'static,
    {
        ensure!(
            request.method() != Method::CONNECT,
            "CONNECT is not supported"
        );
        ensure!(
            !request.headers().contains_key("upgrade"),
            "protocol upgrades are not supported"
        );
        let mut headers = request.headers().clone();
        if let Some(capability_header) = capability_header {
            headers.remove(capability_header);
        }
        let credential_bindings = state.validate_credentials(
            &headers,
            &destination.host,
            destination.scheme == "https",
        )?;
        // The HTTP API always supplies a resolver; Firecracker can rely on its host policy.
        if let Some(resolver) = &state.resolver {
            let authorized = tokio::time::timeout(
                IO_TIMEOUT,
                resolver.authorize(&state.identity, &destination, &credential_bindings),
            )
            .await
            .map_err(|_| anyhow!("request authorization timed out"))?;
            authorized.map_err(|_| anyhow!("request is not authorized"))?;
        }
        strip_hop_headers(&mut headers)?;
        let client = state.client(&destination.host, destination.port).await?;
        state
            .substitute_credentials(&mut headers, &destination)
            .await?;
        relay(request, headers, url, client).await
    }
}

impl State {
    fn new(
        identity: EgressIdentity,
        policy: EgressPolicy,
        resolver: Option<Arc<dyn EgressCredentialResolver>>,
        upstream: Arc<dyn UpstreamResolver>,
    ) -> Result<Self> {
        let SandboxNetworkPolicy::Limited { allowed_hosts } = policy.networking else {
            return Err(anyhow!(
                "egress proxy currently requires limited networking; unrestricted passthrough is not implemented"
            ));
        };
        let hosts = canonical_egress_hosts(&allowed_hosts)?;
        ensure!(
            policy.credentials.is_empty() || resolver.is_some(),
            "credential substitution requires an egress credential resolver"
        );
        let credentials = policy
            .credentials
            .into_iter()
            .map(|binding| EgressRequestCredential {
                binding,
                placeholder: format!("{PLACEHOLDER_PREFIX}{}", uuid::Uuid::new_v4().simple()),
            })
            .collect();
        let mut state = Self::new_request(identity, credentials, resolver, upstream)?;
        state.hosts = hosts;
        Ok(state)
    }

    fn new_request(
        identity: EgressIdentity,
        credentials: Vec<EgressRequestCredential>,
        resolver: Option<Arc<dyn EgressCredentialResolver>>,
        upstream: Arc<dyn UpstreamResolver>,
    ) -> Result<Self> {
        ensure!(
            !identity.sandbox_id.is_empty(),
            "egress identity is required"
        );
        let mut variables = HashSet::new();
        let mut bindings = Vec::new();
        for EgressRequestCredential {
            binding: config,
            placeholder,
        } in credentials
        {
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
            ensure!(
                placeholder.starts_with(PLACEHOLDER_PREFIX)
                    && placeholder.len() > PLACEHOLDER_PREFIX.len()
                    && placeholder
                        .bytes()
                        .all(|c| c == b'_' || c.is_ascii_alphanumeric()),
                "invalid credential placeholder"
            );
            ensure!(
                bindings.iter().all(|binding: &Binding| !binding
                    .placeholder
                    .starts_with(&placeholder)
                    && !placeholder.starts_with(&binding.placeholder)),
                "overlapping credential placeholders"
            );
            bindings.push(Binding {
                config,
                hosts,
                placeholder,
            });
        }
        Ok(Self {
            clients: Mutex::new(HashMap::new()),
            hosts: HashSet::new(),
            bindings,
            identity,
            resolver,
            upstream,
        })
    }

    fn destination(
        &self,
        request: &Request<Incoming>,
        sni: Option<&str>,
    ) -> Result<(EgressDestination, reqwest::Url)> {
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
        ensure!(self.hosts.contains(&host), "host is not allowed");
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
        Ok((egress_destination(&url, request.method())?, url))
    }

    fn validate_credentials(
        &self,
        headers: &HeaderMap,
        host: &str,
        tls: bool,
    ) -> Result<Vec<String>> {
        let mut credential_bindings = HashSet::new();
        for (header, value) in headers {
            if !contains_placeholder(value.as_bytes()) {
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
            for token in placeholder_tokens(value.to_str()?) {
                let binding = self
                    .bindings
                    .iter()
                    .find(|binding| binding.placeholder == token && binding.permits_header(host))
                    .context("credential placeholder does not match this request")?;
                credential_bindings.insert(binding.config.name.clone());
            }
        }
        let mut credential_bindings = credential_bindings.into_iter().collect::<Vec<_>>();
        credential_bindings.sort_unstable();
        Ok(credential_bindings)
    }

    async fn substitute_credentials(
        &self,
        headers: &mut HeaderMap,
        destination: &EgressDestination,
    ) -> Result<()> {
        for (_, header_value) in headers.iter_mut() {
            if !contains_placeholder(header_value.as_bytes()) {
                continue;
            }
            let mut replacement = header_value.to_str()?.to_owned();
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
                        .resolve(&self.identity, &binding.config.name, destination),
                )
                .await?
                .map_err(|_| anyhow!("credential is unavailable or not authorized"))?;
                replacement = replacement.replace(&binding.placeholder, &value);
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

fn contains_placeholder(value: &[u8]) -> bool {
    value
        .windows(PLACEHOLDER_PREFIX.len())
        .any(|s| s == PLACEHOLDER_PREFIX.as_bytes())
}

fn placeholder_tokens(mut value: &str) -> Vec<&str> {
    let mut tokens = Vec::new();
    while let Some(start) = value.find(PLACEHOLDER_PREFIX) {
        let after_prefix = &value[start + PLACEHOLDER_PREFIX.len()..];
        let suffix_len = after_prefix
            .find(|c: char| !c.is_ascii_alphanumeric() && c != '_')
            .unwrap_or(after_prefix.len());
        let end = start + PLACEHOLDER_PREFIX.len() + suffix_len;
        tokens.push(&value[start..end]);
        value = &value[end..];
    }
    tokens
}

async fn relay<B>(
    request: Request<B>,
    mut headers: HeaderMap,
    url: reqwest::Url,
    client: reqwest::Client,
) -> Result<Response<EgressResponseBody>>
where
    B: Body<Data = Bytes> + Send + 'static,
    B::Error: Into<ProxyError> + Send + Sync + 'static,
{
    headers.remove(HOST);
    headers.remove("content-length");
    let (parts, body) = request.into_parts();
    let body = tokio::time::timeout(IO_TIMEOUT, Limited::new(body, MAX_REQUEST_BODY).collect())
        .await?
        .map_err(|_| anyhow!("invalid or oversized request body"))?
        .to_bytes();
    let response = client
        .request(parts.method, url)
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
            let response = match state.destination(&request, sni.as_deref()) {
                Ok((destination, url)) => {
                    EgressEngine::forward(&state, request, destination, url, None).await
                }
                Err(error) => Err(error),
            };
            let response = match response {
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
                    let stream = tokio::time::timeout(IO_TIMEOUT, tls.accept(stream)).await??;
                    let sni = stream.get_ref().1.server_name().context("TLS SNI is required")?.to_owned();
                    http_connection(stream, state, Some(sni)).await
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
