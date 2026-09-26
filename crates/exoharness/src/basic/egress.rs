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
        let mut harness = BasicExoHarness {
            caller: None,
            inner: self
                .harness
                .upgrade()
                .context("egress runtime is unavailable")?,
        };
        let owner_dir = harness.owner_dir(identity.scope);
        let sandbox = load_stored_sandbox(&harness, &owner_dir, &identity.sandbox_id).await?;
        anyhow::ensure!(sandbox.running, "sandbox is not running");
        if let Some(policy) = harness.inner.access_policy.get() {
            harness.caller = Some(crate::access::Caller {
                principal: sandbox
                    .principal
                    .clone()
                    .context("sandbox has no authenticated caller")?,
                policy: policy.clone(),
            });
            harness.check(identity.scope).await?;
        }
        let reference = sandbox
            .credentials
            .get(binding_name)
            .context("sandbox credential is unavailable")?;
        let context = ScopedVaultContext {
            harness: &harness,
            scope: identity.scope,
        };
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
