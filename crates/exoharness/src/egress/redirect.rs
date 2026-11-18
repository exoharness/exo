use super::*;

pub(super) const MAX_REDIRECT_BODY_BYTES: usize = 128 * 1024 * 1024;

/// Authorizes a download URL returned by a trusted upstream, independently of
/// the sandbox's host allowlist. Disabled unless installed on a [`ProxySession`].
///
/// Implementations must authorize the original request (including repository or
/// other resource scope), status, and target. Credential authorization only runs
/// when the sandbox supplies a credential placeholder; it is not sufficient by
/// itself to authorize redirects. Never approve a target just because it is signed.
///
/// Exo follows at most one redirect from an HTTPS GET to another HTTPS URL on
/// port 443, with no original headers, body, or credential substitution. Public
/// DNS filtering and pinning still apply. Downloads are streamed, limited to
/// 128 MiB and 30 seconds, and further redirects fail closed.
#[async_trait]
pub trait EgressRedirectPolicy: Send + Sync {
    async fn allow_redirect(
        &self,
        identity: &EgressIdentity,
        source: &EgressDestination,
        status: StatusCode,
        target: &reqwest::Url,
    ) -> Result<bool, EgressError>;
}

impl State {
    pub(super) async fn follow_redirect(
        &self,
        source: &EgressDestination,
        url: &reqwest::Url,
        response: reqwest::Response,
    ) -> Result<Response<ProxyBody>, EgressError> {
        let Some(policy) = &self.redirect_policy else {
            return Ok(proxy_response(response)?);
        };
        if source.method != Method::GET
            || url.scheme() != "https"
            || !matches!(response.status().as_u16(), 301 | 302 | 303 | 307 | 308)
        {
            return Ok(proxy_response(response)?);
        }
        let location = response
            .headers()
            .get(hyper::header::LOCATION)
            .context("redirect is missing Location")?
            .to_str()
            .context("invalid redirect Location")?;
        let target = url.join(location).context("invalid redirect URL")?;
        if !tokio::time::timeout(
            IO_TIMEOUT,
            policy.allow_redirect(&self.identity, source, response.status(), &target),
        )
        .await
        .context("redirect authorization timed out")??
        {
            return Ok(proxy_response(response)?);
        }
        if target.scheme() != "https"
            || target.port_or_known_default() != Some(443)
            || !target.username().is_empty()
            || target.password().is_some()
            || target.fragment().is_some()
        {
            return Err(anyhow!(
                "redirect requires HTTPS on port 443 without userinfo or fragment"
            )
            .into());
        }
        let host = canonical_egress_host(target.host_str().context("missing redirect host")?)?;
        self.check_port(443)?;
        drop(response);
        // Only this upstream-provided URL bypasses the host allowlist. Using the
        // normal client keeps DNS filtering/pinning and disables redirects.
        let client = self.client(&host, 443).await?;
        let response = client
            .get(target)
            .timeout(IO_TIMEOUT)
            .send()
            .await
            .map_err(|_| anyhow!("redirect download failed"))?;
        if response.status().is_redirection() {
            return Err(anyhow!("further redirects are not permitted").into());
        }
        if response
            .content_length()
            .is_some_and(|size| size > MAX_REDIRECT_BODY_BYTES as u64)
        {
            return Err(anyhow!("redirect response is too large").into());
        }
        Ok(proxy_response(response)?
            .map(|body| Limited::new(body, MAX_REDIRECT_BODY_BYTES).boxed()))
    }
}
