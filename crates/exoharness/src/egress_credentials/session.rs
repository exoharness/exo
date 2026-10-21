//! Persistent interception session over Exo's ordinary store and vaults.
use super::{
    CredentialBindings, EgressCredentialResolver, EgressDestination, EgressIdentity, NetworkPolicy,
    credential_fingerprint, denied,
};
use crate::{ConversationHandle, EgressPolicy, ExoHarness, ResourceScope, SandboxRequest, Storage};
use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use http::HeaderMap;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, sync::Arc};

#[derive(Serialize, Deserialize)]
pub struct EgressSession {
    identity: EgressIdentity,
    policy: EgressPolicy,
    environment: HashMap<String, String>,
}
impl EgressSession {
    fn key(sandbox: &str) -> String {
        format!("sandbox/{sandbox}/egress.json")
    }
    pub async fn configure(
        state: &dyn ExoHarness,
        storage: &dyn Storage,
        request: SandboxRequest,
    ) -> Result<HashMap<String, String>> {
        let identity = EgressIdentity {
            sandbox_id: request.sandbox_id,
            scope: request.scope,
        };
        thread(state, &identity).await?;
        let key = Self::key(&identity.sandbox_id);
        let previous = storage
            .get(&key)
            .await?
            .map(|bytes| serde_json::from_slice::<Self>(&bytes))
            .transpose()?;
        let policy = request.spec.policy;
        let mut environment =
            CredentialBindings::new(policy.credentials.clone(), None)?.environment();
        if let Some(previous) = previous {
            ensure!(previous.identity == identity, "sandbox identity mismatch");
            for binding in &policy.credentials {
                if previous.policy.credentials.contains(binding)
                    && let Some(value) = previous.environment.get(&binding.environment_variable)
                {
                    environment.insert(binding.environment_variable.clone(), value.clone());
                }
            }
        }
        let session = Self {
            identity,
            policy,
            environment,
        };
        storage
            .put(&key, serde_json::to_vec(&session)?, false)
            .await?;
        Ok(session.environment)
    }
    pub async fn load(storage: &dyn Storage, identity: &EgressIdentity) -> Result<Self> {
        let bytes = storage
            .get(&Self::key(&identity.sandbox_id))
            .await?
            .context("sandbox egress is not configured")?;
        let session: Self = serde_json::from_slice(&bytes)?;
        ensure!(&session.identity == identity, "sandbox identity mismatch");
        Ok(session)
    }
    pub async fn rewrite(
        &self,
        state: &dyn ExoHarness,
        headers: &mut HeaderMap,
        destination: &EgressDestination,
        tls: bool,
        rejected: Vec<(String, String)>,
    ) -> Result<(Vec<(String, String)>, bool)> {
        NetworkPolicy::new(&self.policy)?
            .authorize(destination)
            .map_err(denied)?;
        let bindings =
            CredentialBindings::new(self.policy.credentials.clone(), Some(&self.environment))?;
        let resolver = Resolver {
            conversation: thread(state, &self.identity).await?,
            rejected,
        };
        let values = bindings
            .rewrite_headers(headers, destination, tls, &self.identity, &resolver)
            .await?;
        let credentials: Vec<_> = values
            .into_iter()
            .map(|(name, value)| (name, credential_fingerprint(&value)))
            .collect();
        let refreshed = credentials.iter().any(|(name, hash)| {
            resolver
                .rejected
                .iter()
                .any(|(binding, old)| binding == name && old != hash)
        });
        Ok((credentials, refreshed))
    }
}
async fn thread(
    state: &dyn ExoHarness,
    identity: &EgressIdentity,
) -> Result<Arc<dyn ConversationHandle>> {
    let ResourceScope::Thread {
        agent_id,
        thread_id,
    } = identity.scope
    else {
        anyhow::bail!("egress requires a thread")
    };
    state
        .get_agent(&agent_id)
        .await?
        .context("agent not found")?
        .get_thread(&thread_id)
        .await?
        .context("thread not found")
}
struct Resolver {
    conversation: Arc<dyn ConversationHandle>,
    rejected: Vec<(String, String)>,
}
#[async_trait]
impl EgressCredentialResolver for Resolver {
    async fn resolve(
        &self,
        _identity: &EgressIdentity,
        name: &str,
        destination: &EgressDestination,
    ) -> Result<String> {
        let reference = crate::vault::find_secret(self.conversation.as_ref(), name)
            .await?
            .ok_or_else(|| denied(anyhow::anyhow!("credential not found")))?;
        super::vault::resolve_with_rejection(
            self.conversation.as_ref(),
            &reference,
            destination,
            self.rejected
                .iter()
                .find(|(binding, _)| binding == name)
                .map(|(_, hash)| super::vault::RejectedCredential::Fingerprint(hash)),
        )
        .await
    }
}

#[cfg(all(test, feature = "basic-backend"))]
mod tests {
    use super::*;
    use crate::{
        BasicExoHarness, CredentialInjectionLocation, CredentialNetworkPolicy,
        EgressCredentialBinding, NewAgentRequest, SandboxNetworkPolicy,
    };

    #[tokio::test]
    async fn missing_credentials_and_protocol_upgrades_are_policy_denials() -> Result<()> {
        let state = BasicExoHarness::in_memory(crate::test_support::local_test_config(
            "unused-egress-session",
        ))
        .await?;
        let agent = state
            .new_agent(NewAgentRequest {
                slug: "egress".into(),
                name: "Egress".into(),
                vaults: vec![],
            })
            .await?;
        let conversation = agent.new_conversation(Default::default()).await?;
        let policy = EgressPolicy {
            networking: SandboxNetworkPolicy::Unrestricted,
            allowed_tcp_ports: None,
            credentials: vec![EgressCredentialBinding {
                name: "missing".into(),
                environment_variable: "TOKEN".into(),
                networking: CredentialNetworkPolicy::Limited {
                    allowed_hosts: vec!["api.test".into()],
                },
                injection_location: CredentialInjectionLocation { header: true },
            }],
        };
        let session = EgressSession {
            identity: EgressIdentity {
                sandbox_id: "sandbox".into(),
                scope: ResourceScope::Thread {
                    agent_id: agent.record().id,
                    thread_id: conversation.record().id,
                },
            },
            environment: CredentialBindings::new(policy.credentials.clone(), None)?.environment(),
            policy,
        };
        let destination = super::super::http_destination(
            &url::Url::parse("https://api.test/")?,
            http::Method::GET,
        )?;
        for (name, value) in [
            (
                "authorization",
                format!("Bearer {}", session.environment["TOKEN"]),
            ),
            ("upgrade", "websocket".into()),
        ] {
            let mut headers = HeaderMap::new();
            headers.insert(
                http::header::HeaderName::from_bytes(name.as_bytes())?,
                http::HeaderValue::from_str(&value)?,
            );
            let error = session
                .rewrite(&state, &mut headers, &destination, true, vec![])
                .await
                .expect_err("policy denial");
            assert!(error.is::<super::super::EgressDenied>(), "{error:#}");
        }
        Ok(())
    }
}
