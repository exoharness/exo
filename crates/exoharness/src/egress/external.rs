use super::*;
use serde::{Deserialize, Serialize};
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
