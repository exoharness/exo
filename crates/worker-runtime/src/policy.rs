//! Platform HTTP interception using Exo's shared policy and vault implementation.
use crate::host::{Host, HostStorage};
use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use executor::Runtime;
use exoharness::egress_credentials::{
    CredentialBindings, EgressCredentialResolver, EgressDestination, EgressIdentity,
    strip_hop_headers,
};
use exoharness::{
    AgentId, ConversationHandle, EgressPolicy, ResourceScope, SandboxNetworkPolicy, Storage,
    ThreadId,
};
use http::{
    HeaderMap, Method,
    header::{HeaderName, HeaderValue},
};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, sync::Arc};

#[derive(Serialize, Deserialize)]
struct Session {
    policy: EgressPolicy,
    agent_id: AgentId,
    thread_id: ThreadId,
    environment: HashMap<String, String>,
}

pub(crate) async fn environment(
    runtime: &Runtime,
    host: &Arc<Host>,
    request: exoharness::SandboxRequest,
) -> Result<HashMap<String, String>> {
    let ResourceScope::Thread {
        agent_id,
        thread_id,
    } = request.scope
    else {
        anyhow::bail!("Cloudflare egress requires a thread");
    };
    let agent = runtime
        .exoharness_handle()
        .get_agent(&agent_id)
        .await?
        .context("agent not found")?;
    agent
        .get_thread(&thread_id)
        .await?
        .context("thread not found")?;
    let storage = HostStorage(host.clone());
    let key = format!("sandbox/{}/egress.json", request.sandbox_id);
    let saved = storage
        .get(&key)
        .await?
        .map(|bytes| serde_json::from_slice::<Session>(&bytes))
        .transpose()?;
    let policy = request.spec.policy;
    let mut environment = CredentialBindings::new(policy.credentials.clone(), None)?.environment();
    if let Some(saved) = &saved {
        ensure!(
            saved.agent_id == agent_id && saved.thread_id == thread_id,
            "sandbox identity mismatch"
        );
        for binding in &policy.credentials {
            if saved.policy.credentials.contains(binding)
                && let Some(value) = saved.environment.get(&binding.environment_variable)
            {
                environment.insert(binding.environment_variable.clone(), value.clone());
            }
        }
    }
    let session = Session {
        agent_id,
        thread_id,
        policy,
        environment,
    };
    storage
        .put(&key, serde_json::to_vec(&session)?, false)
        .await?;
    Ok(session.environment)
}

async fn session(
    runtime: &Runtime,
    host: &Arc<Host>,
    agent_id: AgentId,
    thread_id: ThreadId,
    sandbox_id: &str,
) -> Result<(Arc<dyn ConversationHandle>, Session)> {
    let agent = runtime
        .exoharness_handle()
        .get_agent(&agent_id)
        .await?
        .context("agent not found")?;
    let thread = agent
        .get_thread(&thread_id)
        .await?
        .context("thread not found")?;
    let bytes = HostStorage(host.clone())
        .get(&format!("sandbox/{sandbox_id}/egress.json"))
        .await?
        .context("sandbox egress is not configured")?;
    let session: Session = serde_json::from_slice(&bytes)?;
    ensure!(
        session.agent_id == agent_id && session.thread_id == thread_id,
        "sandbox identity mismatch"
    );
    Ok((thread, session))
}

struct Resolver(Arc<dyn ConversationHandle>);
#[async_trait]
impl EgressCredentialResolver for Resolver {
    async fn resolve(
        &self,
        _identity: &EgressIdentity,
        name: &str,
        destination: &EgressDestination,
    ) -> Result<String> {
        let reference = exoharness::vault::find_secret(self.0.as_ref(), name)
            .await?
            .context("credential not found")?;
        exoharness::egress_credentials::vault::resolve_credential(
            self.0.as_ref(),
            &reference,
            destination,
            None,
        )
        .await
    }
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
) -> Result<Vec<(String, String)>> {
    let url = url::Url::parse(url)?;
    ensure!(
        matches!(url.scheme(), "http" | "https")
            && url.username().is_empty()
            && url.password().is_none()
            && url.fragment().is_none(),
        "invalid HTTP destination"
    );
    let hostname = url.host_str().context("missing destination hostname")?;
    ensure!(
        matches!(url.host(), Some(url::Host::Domain(_)))
            && hostname != "localhost"
            && ![".localhost", ".internal", ".local"]
                .iter()
                .any(|suffix| hostname.ends_with(suffix)),
        "egress requires a public DNS hostname"
    );
    let port = url
        .port_or_known_default()
        .context("missing destination port")?;
    ensure!(
        matches!((url.scheme(), port), ("http", 80) | ("https", 443)),
        "the sandbox intercepts HTTP port 80 and HTTPS port 443"
    );
    let (conversation, session) = session(runtime, host, agent, thread, sandbox_id).await?;
    ensure!(session.policy.allows_tcp_port(port), "network port denied");
    ensure!(
        match &session.policy.networking {
            SandboxNetworkPolicy::Disabled => false,
            SandboxNetworkPolicy::Unrestricted => true,
            SandboxNetworkPolicy::Limited { allowed_hosts } => allowed_hosts
                .iter()
                .any(|allowed| allowed.eq_ignore_ascii_case(hostname)),
        },
        "network destination denied"
    );
    let bindings = CredentialBindings::new(session.policy.credentials, Some(&session.environment))?;
    let mut map = HeaderMap::new();
    for (name, value) in headers {
        map.append(
            HeaderName::from_bytes(name.as_bytes())?,
            HeaderValue::from_str(&value)?,
        );
    }
    let destination = EgressDestination {
        host: hostname.into(),
        port,
        method: method.parse::<Method>()?,
        path: url[url::Position::BeforePath..url::Position::AfterQuery].into(),
    };
    bindings.validate(&map, &destination, url.scheme() == "https")?;
    // Remove Connection-nominated fields before resolving any secrets.
    strip_hop_headers(&mut map)?;
    bindings
        .substitute(
            &mut map,
            &destination,
            &EgressIdentity {
                sandbox_id: sandbox_id.into(),
                scope: ResourceScope::Thread {
                    agent_id: agent,
                    thread_id: thread,
                },
            },
            &Resolver(conversation),
        )
        .await?;
    map.remove(http::header::HOST);
    map.remove(http::header::CONTENT_LENGTH);
    map.iter()
        .map(|(name, value)| Ok((name.to_string(), value.to_str()?.to_owned())))
        .collect()
}
