use super::*;
use crate::egress::{EgressCredentialResolver, EgressDestination, EgressIdentity};

pub(super) struct LocalEgressResolver {
    pub harness: std::sync::Weak<BasicExoHarnessInner>,
}

impl LocalEgressResolver {
    async fn resolve_value(
        &self,
        identity: &EgressIdentity,
        binding_name: &str,
        destination: &EgressDestination,
        rejected: Option<&str>,
    ) -> Result<String> {
        let harness = BasicExoHarness {
            inner: self
                .harness
                .upgrade()
                .context("egress runtime is unavailable")?,
        };
        let owner_dir = harness.owner_dir(identity.scope);
        let sandbox = load_stored_sandbox(&harness, &owner_dir, &identity.sandbox_id).await?;
        anyhow::ensure!(sandbox.running, "sandbox is not running");
        let reference = sandbox
            .credentials
            .get(binding_name)
            .context("sandbox credential is unavailable")?;
        let context = ScopedVaultContext {
            harness: &harness,
            scope: identity.scope,
        };
        let policy = sandbox.policy();
        let credential = policy
            .credentials
            .iter()
            .find(|binding| binding.name == binding_name)
            .context("sandbox credential policy is unavailable")?;
        if let Some(model_id) = credential.model {
            let model = context.model_binding(&model_id).await?;
            let endpoint = crate::vault::model_endpoint(
                model.base_url.as_deref(),
                &credential.environment_variable,
            )?;
            anyhow::ensure!(
                model.secret.as_ref() == Some(reference),
                "sandbox model credential changed; create a new sandbox"
            );
            let path = endpoint.path().trim_end_matches('/');
            anyhow::ensure!(
                endpoint.host_str() == Some(destination.host.as_str())
                    && endpoint.port_or_known_default() == Some(destination.port)
                    && (path.is_empty()
                        || destination.path == path
                        || destination.path.starts_with(&format!("{path}/"))),
                "request is outside the model endpoint"
            );
            let vault =
                crate::vault::model_credential_vault(&context, reference, &endpoint).await?;
            return match vault
                .get_secret(&reference.secret_id)
                .await?
                .context("sandbox model credential is unavailable")?
            {
                Secret::Key { value } => Ok(value),
                Secret::Oauth { .. } => {
                    bail!("sandbox model credentials currently require an API key")
                }
            };
        }
        crate::egress::vault::resolve_credential(&context, reference, destination, rejected).await
    }
}

#[async_trait]
impl EgressCredentialResolver for LocalEgressResolver {
    async fn resolve(
        &self,
        identity: &EgressIdentity,
        name: &str,
        destination: &EgressDestination,
    ) -> Result<String> {
        self.resolve_value(identity, name, destination, None).await
    }

    async fn refresh(
        &self,
        identity: &EgressIdentity,
        name: &str,
        destination: &EgressDestination,
        rejected: &str,
    ) -> Result<Option<String>> {
        let value = self
            .resolve_value(identity, name, destination, Some(rejected))
            .await?;
        Ok((value != rejected).then_some(value))
    }
}

#[cfg(test)]
#[path = "egress/tests.rs"]
mod tests;
