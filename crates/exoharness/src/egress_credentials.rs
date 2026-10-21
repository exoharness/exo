//! Credential substitution shared by native proxies and hosted runtimes.
use crate::EgressCredentialBinding;
use anyhow::{Context, Result, anyhow, ensure};
use async_trait::async_trait;
use base64::Engine;
use http::{
    Method,
    header::{CONNECTION, HOST, HeaderMap, HeaderName, HeaderValue},
};
use std::collections::{HashMap, HashSet};
pub mod session;
pub mod vault;
pub const PLACEHOLDER_PREFIX: &str = "exo_egress_";

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct EgressIdentity {
    pub sandbox_id: String,
    pub scope: crate::ResourceScope,
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

    async fn refresh(
        &self,
        _identity: &EgressIdentity,
        _binding_name: &str,
        _destination: &EgressDestination,
        _rejected: &str,
    ) -> Result<Option<String>> {
        Ok(None)
    }
}

pub(crate) struct Binding {
    pub(crate) config: EgressCredentialBinding,
    hosts: HashSet<String>,
    pub(crate) placeholder: String,
}

impl Binding {
    pub(crate) fn permits_header(&self, destination: &EgressDestination) -> bool {
        self.config.injection_location.header
            && self.hosts.contains(&destination.host)
            && crate::CredentialDestination::url(&format!(
                "https://{}:{}{}",
                destination.host, destination.port, destination.path
            ))
            .is_ok_and(|destination| self.config.networking.permits(&destination))
    }
}

pub(crate) fn prepare_bindings(
    credentials: Vec<EgressCredentialBinding>,
    placeholders: Option<&HashMap<String, String>>,
) -> Result<Vec<Binding>> {
    let mut variables = HashSet::new();
    let mut values = HashSet::new();
    let mut bindings = Vec::new();
    for mut config in credentials {
        config.networking = config.networking.normalized()?;
        let hosts = config.networking.hosts()?;
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
    Ok(bindings)
}

pub(crate) fn validate_credentials(
    bindings: &[Binding],
    headers: &HeaderMap,
    destination: &EgressDestination,
    tls: bool,
) -> Result<()> {
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
        for binding in bindings {
            if binding.permits_header(destination) {
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

pub(crate) async fn substitute_credentials(
    bindings: &[Binding],
    headers: &mut HeaderMap,
    destination: &EgressDestination,
    identity: &EgressIdentity,
    resolver: &dyn EgressCredentialResolver,
) -> Result<Vec<(String, String)>> {
    let mut credentials = Vec::new();
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
        for binding in bindings {
            if !binding.permits_header(destination) || !replacement.contains(&binding.placeholder) {
                continue;
            }
            let value = resolver
                .resolve(identity, &binding.config.name, destination)
                .await
                .context("credential is unavailable or not authorized")?;
            replacement = replacement.replace(&binding.placeholder, &value);
            credentials.push((binding.config.name.clone(), value));
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
    Ok(credentials)
}

fn basic_credential_placeholder(
    header: &HeaderName,
    value: &HeaderValue,
) -> Result<Option<String>> {
    if header == http::header::AUTHORIZATION
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

pub(crate) fn hop_header(name: &HeaderName) -> bool {
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

pub fn strip_hop_headers(headers: &mut HeaderMap) -> Result<()> {
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

/// Validated header bindings and their opaque sandbox environment values.
pub struct CredentialBindings {
    bindings: Vec<Binding>,
}
impl CredentialBindings {
    pub fn new(
        credentials: Vec<EgressCredentialBinding>,
        placeholders: Option<&HashMap<String, String>>,
    ) -> Result<Self> {
        Ok(Self {
            bindings: prepare_bindings(credentials, placeholders)?,
        })
    }
    pub fn environment(&self) -> HashMap<String, String> {
        self.bindings
            .iter()
            .map(|binding| {
                (
                    binding.config.environment_variable.clone(),
                    binding.placeholder.clone(),
                )
            })
            .collect()
    }
    pub fn validate(
        &self,
        headers: &HeaderMap,
        destination: &EgressDestination,
        tls: bool,
    ) -> Result<()> {
        validate_credentials(&self.bindings, headers, destination, tls)
    }
    pub async fn substitute(
        &self,
        headers: &mut HeaderMap,
        destination: &EgressDestination,
        identity: &EgressIdentity,
        resolver: &dyn EgressCredentialResolver,
    ) -> Result<()> {
        self.substitute_tracked(headers, destination, identity, resolver)
            .await?;
        Ok(())
    }

    /// The substituted values let the proxy refresh only credentials rejected by the upstream.
    pub async fn substitute_tracked(
        &self,
        headers: &mut HeaderMap,
        destination: &EgressDestination,
        identity: &EgressIdentity,
        resolver: &dyn EgressCredentialResolver,
    ) -> Result<Vec<(String, String)>> {
        substitute_credentials(&self.bindings, headers, destination, identity, resolver).await
    }
}

/// Only policy rejections receive 403; I/O and corrupt stored state remain errors.
#[derive(Debug, thiserror::Error)]
#[error("egress request denied")]
pub struct EgressDenied;
pub fn denied(error: impl Into<anyhow::Error>) -> anyhow::Error {
    error.into().context(EgressDenied)
}

pub fn credential_fingerprint(value: &str) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(value.as_bytes()))
}

/// Canonical HTTP destination shared by transparent and platform interception.
pub fn http_destination(url: &url::Url, method: Method) -> Result<EgressDestination> {
    ensure!(
        matches!(url.scheme(), "http" | "https")
            && url.username().is_empty()
            && url.password().is_none()
            && url.fragment().is_none(),
        "invalid HTTP destination"
    );
    let host = crate::types::canonical_egress_host(url.host_str().context("missing hostname")?)?;
    let port = url
        .port_or_known_default()
        .context("missing destination port")?;
    ensure!(
        matches!((url.scheme(), port), ("http", 80) | ("https", 443)),
        "only standard HTTP ports are supported"
    );
    Ok(EgressDestination {
        host,
        port,
        method,
        path: url[url::Position::BeforePath..url::Position::AfterQuery].into(),
    })
}

/// Compiled network rules used by native listeners and hosted interception.
pub(crate) struct NetworkPolicy {
    hosts: HashSet<String>,
    unrestricted: bool,
    enabled: bool,
    ports: Option<Vec<u16>>,
}
impl NetworkPolicy {
    pub(crate) fn new(policy: &crate::EgressPolicy) -> Result<Self> {
        ensure!(
            policy
                .allowed_tcp_ports
                .as_ref()
                .is_none_or(|ports| !ports.contains(&0)),
            "policy.allowed_tcp_ports cannot contain port zero"
        );
        let hosts = match &policy.networking {
            crate::SandboxNetworkPolicy::Limited { allowed_hosts } => {
                crate::types::canonical_egress_hosts(allowed_hosts)?
            }
            _ => HashSet::new(),
        };
        Ok(Self {
            hosts,
            unrestricted: policy.networking == crate::SandboxNetworkPolicy::Unrestricted,
            enabled: policy.networking_enabled(),
            ports: policy.allowed_tcp_ports.clone(),
        })
    }
    #[cfg(feature = "basic-backend")]
    pub(crate) fn hosts(&self) -> &HashSet<String> {
        &self.hosts
    }
    #[cfg(feature = "basic-backend")]
    pub(crate) fn unrestricted(&self) -> bool {
        self.unrestricted
    }
    pub(crate) fn permits_host(&self, host: &str) -> bool {
        self.enabled && (self.unrestricted || self.hosts.contains(host))
    }
    pub(crate) fn check_port(&self, port: u16) -> Result<()> {
        ensure!(
            port != 0
                && self
                    .ports
                    .as_ref()
                    .is_none_or(|ports| ports.contains(&port)),
            "TCP port is not allowed"
        );
        Ok(())
    }
    pub(crate) fn authorize(&self, destination: &EgressDestination) -> Result<()> {
        self.check_port(destination.port)?;
        ensure!(
            self.permits_host(&destination.host),
            "network destination denied"
        );
        Ok(())
    }
}

pub(crate) async fn rewrite_headers(
    bindings: &[Binding],
    headers: &mut HeaderMap,
    destination: &EgressDestination,
    tls: bool,
    identity: &EgressIdentity,
    resolver: &dyn EgressCredentialResolver,
) -> Result<Vec<(String, String)>> {
    if headers.contains_key(http::header::UPGRADE) {
        return Err(denied(anyhow!("protocol upgrades are not supported")));
    }
    if destination.method == Method::CONNECT {
        return Err(denied(anyhow!("CONNECT is not supported")));
    }
    validate_credentials(bindings, headers, destination, tls).map_err(denied)?;
    strip_hop_headers(headers).map_err(denied)?;
    let credentials =
        substitute_credentials(bindings, headers, destination, identity, resolver).await?;
    headers.remove(HOST);
    headers.remove("content-length");
    Ok(credentials)
}
impl CredentialBindings {
    pub async fn rewrite_headers(
        &self,
        headers: &mut HeaderMap,
        destination: &EgressDestination,
        tls: bool,
        identity: &EgressIdentity,
        resolver: &dyn EgressCredentialResolver,
    ) -> Result<Vec<(String, String)>> {
        rewrite_headers(
            &self.bindings,
            headers,
            destination,
            tls,
            identity,
            resolver,
        )
        .await
    }
}
