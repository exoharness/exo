//! Platform HTTP interception using Exo's shared policy and vault implementation.
use crate::host::{Host, HostStorage};
use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use executor::{AgentConfig, ConversationConfig, Runtime};
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
    environment: HashMap<String, String>,
}

async fn session(
    runtime: &Runtime,
    host: &Arc<Host>,
    agent_id: AgentId,
    thread_id: ThreadId,
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
    let agent_config: AgentConfig = executor::load_agent_config(agent.as_ref()).await?;
    let config: ConversationConfig = executor::load_conversation_config(thread.as_ref()).await?;
    let policy = executor::sandbox_policy::sandbox_policy(thread.as_ref(), &agent_config, &config)
        .await?
        .unwrap_or_else(|| {
            if agent_config.sandbox.enable_networking {
                SandboxNetworkPolicy::Unrestricted.into()
            } else {
                SandboxNetworkPolicy::Disabled.into()
            }
        });
    let storage = HostStorage(host.clone());
    let key = format!("sandbox/{thread_id}/egress.json");
    let saved = storage
        .get(&key)
        .await?
        .map(|bytes| serde_json::from_slice::<Session>(&bytes))
        .transpose()?;
    // Processes retain their environment while warm, including across provider
    // eviction. Reuse opaque values only for an unchanged credential binding.
    let fresh = CredentialBindings::new(policy.credentials.clone(), None)?.environment();
    let mut environment = fresh;
    if let Some(saved) = &saved {
        for binding in &policy.credentials {
            if saved.policy.credentials.contains(binding)
                && let Some(value) = saved.environment.get(&binding.environment_variable)
            {
                environment.insert(binding.environment_variable.clone(), value.clone());
            }
        }
    }
    let session = Session {
        policy,
        environment,
    };
    if saved.as_ref().is_none_or(|saved| {
        saved.policy != session.policy || saved.environment != session.environment
    }) {
        storage
            .put(&key, serde_json::to_vec(&session)?, false)
            .await?;
    }
    Ok((thread, session))
}

pub(crate) async fn environment(
    runtime: &Runtime,
    host: &Arc<Host>,
    agent: AgentId,
    thread: ThreadId,
) -> Result<HashMap<String, String>> {
    Ok(session(runtime, host, agent, thread).await?.1.environment)
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
    let (conversation, session) = session(runtime, host, agent, thread).await?;
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
                sandbox_id: thread.to_string(),
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
