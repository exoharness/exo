//! Cloudflare header encoding and platform destination restrictions.
use crate::host::{Host, HostStorage};
use anyhow::{Result, ensure};
use executor::Runtime;
use exoharness::egress_credentials::{
    EgressIdentity, denied, http_destination, session::EgressSession,
};
use exoharness::{AgentId, ResourceScope, ThreadId};
use http::{
    HeaderMap,
    header::{HeaderName, HeaderValue},
};
use serde::Serialize;
use std::{collections::HashMap, sync::Arc};

pub(crate) async fn environment(
    runtime: &Runtime,
    host: &Arc<Host>,
    request: exoharness::SandboxRequest,
) -> Result<HashMap<String, String>> {
    EgressSession::configure(
        runtime.exoharness_handle().as_ref(),
        &HostStorage(host.clone()),
        request,
    )
    .await
}
#[derive(Serialize)]
#[serde(untagged)]
pub(crate) enum ProxyReply {
    Allowed(ProxyHeaders),
    Error { status: u16, error: String },
}
#[derive(Serialize)]
pub(crate) struct ProxyHeaders {
    headers: Vec<(String, String)>,
    credentials: Vec<(String, String)>,
    refreshed: bool,
}
pub(crate) async fn proxy_headers(
    runtime: &Runtime,
    host: &Arc<Host>,
    agent: AgentId,
    thread: ThreadId,
    sandbox_id: &str,
    url: &str,
    method: &str,
    headers: Vec<(String, String)>,
    rejected: Option<Vec<(String, String)>>,
) -> Result<ProxyHeaders> {
    let url = url::Url::parse(url).map_err(denied)?;
    let destination = http_destination(&url, method.parse().map_err(denied)?).map_err(denied)?;
    // Cloudflare intercepts public DNS HTTP destinations. DNS/IP resolution
    // itself belongs to the platform; native interception pins its own answers.
    (|| -> Result<()> {
        let host = &destination.host;
        ensure!(
            matches!(url.host(), Some(url::Host::Domain(_)))
                && host != "localhost"
                && ![".localhost", ".internal", ".local"]
                    .iter()
                    .any(|suffix| host.ends_with(suffix)),
            "egress requires a public DNS hostname"
        );
        Ok(())
    })()
    .map_err(denied)?;
    let identity = EgressIdentity {
        sandbox_id: sandbox_id.into(),
        scope: ResourceScope::Thread {
            agent_id: agent,
            thread_id: thread,
        },
    };
    let session = EgressSession::load(&HostStorage(host.clone()), &identity).await?;
    let mut map = HeaderMap::new();
    for (name, value) in headers {
        let bytes = value
            .chars()
            .map(|c| u8::try_from(u32::from(c)))
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(denied)?;
        map.append(
            HeaderName::from_bytes(name.as_bytes()).map_err(denied)?,
            HeaderValue::from_bytes(&bytes).map_err(denied)?,
        );
    }
    let (credentials, refreshed) = session
        .rewrite(
            runtime.exoharness_handle().as_ref(),
            &mut map,
            &destination,
            url.scheme() == "https",
            rejected.unwrap_or_default(),
        )
        .await?;
    let headers = map
        .iter()
        .map(|(name, value)| {
            (
                name.to_string(),
                value.as_bytes().iter().map(|&b| char::from(b)).collect(),
            )
        })
        .collect();
    Ok(ProxyHeaders {
        headers,
        credentials,
        refreshed,
    })
}
