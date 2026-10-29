use super::*;
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use tokio::net::TcpStream;

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExternalProxyConfig {
    pub url: url::Url,
    pub username: String,
    pub password: String,
    pub ca_pem: String,
    pub environment: HashMap<String, String>,
}

impl std::fmt::Debug for ExternalProxyConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExternalProxyConfig")
            .finish_non_exhaustive()
    }
}

impl ExternalProxyConfig {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.url.scheme() == "http"
                && self.url.host_str().is_some()
                && self.url.username().is_empty()
                && self.url.password().is_none()
                && self.url.path() == "/"
                && self.url.query().is_none()
                && self.url.fragment().is_none(),
            "external proxy URL must be an HTTP origin"
        );
        ensure!(
            !self.username.is_empty()
                && !self.username.contains([':', '\r', '\n'])
                && !self.password.is_empty()
                && !self.password.contains(['\r', '\n'])
                && self.username.len() + self.password.len() <= 8192,
            "invalid external proxy authorization"
        );
        reqwest::Certificate::from_pem(self.ca_pem.as_bytes())
            .context("invalid external proxy CA certificate")?;
        Ok(())
    }

    /// The proxy URL carrying this configuration's authorization, for guests
    /// that reach the proxy themselves.
    pub(crate) fn authorized_url(&self) -> Result<url::Url> {
        let mut url = self.url.clone();
        url.set_username(&self.username)
            .and_then(|()| url.set_password(Some(&self.password)))
            .map_err(|()| anyhow!("external proxy URL cannot carry authorization"))?;
        Ok(url)
    }

    pub(super) async fn connect(&self, host: &str) -> Result<crate::BoxSandboxTcpStream> {
        let stream = tokio::time::timeout(
            CONNECT_TIMEOUT,
            TcpStream::connect((
                self.url
                    .host_str()
                    .context("external proxy host is required")?,
                self.url
                    .port_or_known_default()
                    .context("external proxy port is required")?,
            )),
        )
        .await
        .context("external proxy connection timed out")??;
        tokio::time::timeout(IO_TIMEOUT, async {
            let (mut client, connection) = hyper::client::conn::http1::Builder::new()
                .max_buf_size(16 * 1024)
                .handshake(hyper_util::rt::TokioIo::new(stream))
                .await?;
            tokio::spawn(async move {
                if let Err(error) = connection.with_upgrades().await {
                    tracing::debug!(%error, "external proxy connection closed");
                }
            });
            let authority = format!("{host}:443");
            let authorization = base64::engine::general_purpose::STANDARD
                .encode(format!("{}:{}", self.username, self.password));
            let request = Request::connect(&authority)
                .header(HOST, &authority)
                .header("proxy-authorization", format!("Basic {authorization}"))
                .body(http_body_util::Empty::<Bytes>::new())?;
            let response = client.send_request(request).await?;
            ensure!(
                response.status() == StatusCode::OK,
                "external proxy rejected the tunnel"
            );
            let stream = hyper_util::rt::TokioIo::new(hyper::upgrade::on(response).await?);
            Ok::<crate::BoxSandboxTcpStream, anyhow::Error>(Box::pin(stream))
        })
        .await
        .context("external proxy handshake timed out")?
    }
}

/// External proxy settings installed in a guest that reaches the proxy itself,
/// for backends without an in-sandbox egress proxy. The guest trusts the proxy's
/// CA from `ca_path`, and every command inherits the proxy environment.
#[derive(Clone)]
pub(crate) struct DirectExternalProxy {
    proxy: ExternalProxyConfig,
    ca_path: String,
}

impl DirectExternalProxy {
    pub(crate) fn new(proxy: ExternalProxyConfig, ca_path: String) -> Self {
        Self { proxy, ca_path }
    }

    pub(crate) async fn install(&self, handle: &dyn crate::ManagedSandboxHandle) -> Result<()> {
        super::sandbox::install_trust(handle, &self.ca_path, &self.proxy.ca_pem).await
    }

    pub(crate) fn environment(&self) -> Result<HashMap<String, String>> {
        let mut environment = self.proxy.environment.clone();
        environment.extend(guest_proxy_environment(
            self.proxy.authorized_url()?.as_str(),
            &self.ca_path,
        ));
        Ok(environment)
    }

    pub(crate) fn environment_of(egress: Option<&Self>) -> Result<HashMap<String, String>> {
        egress.map_or_else(|| Ok(HashMap::new()), Self::environment)
    }

    pub(crate) fn prepare_command<'a>(
        egress: Option<&Self>,
        command: &'a crate::SandboxCommand,
    ) -> Result<Cow<'a, crate::SandboxCommand>> {
        let Some(egress) = egress else {
            return Ok(Cow::Borrowed(command));
        };
        let mut command = command.clone();
        command.env.extend(egress.environment()?);
        Ok(Cow::Owned(command))
    }
}

impl State {
    pub(super) fn with_external_proxy(
        identity: EgressIdentity,
        policy: EgressPolicy,
        proxy: ExternalProxyConfig,
    ) -> Result<Self> {
        Self::new_with_placeholders(
            identity,
            policy,
            None,
            Arc::new(PublicUpstreamResolver),
            None,
            Some(proxy),
        )
    }
}
