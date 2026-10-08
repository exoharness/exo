use crate::harness_helpers::*;
use anyhow::Result;
use exoharness::ExoHarness;
use exoharness::{
    BasicExoHarness, BasicExoHarnessConfig, NewAgentRequest, NewThreadRequest, PutSecretRequest,
    SandboxBackendRegistration, SandboxProvider, Secret, SecretBackendChoice,
};

#[tokio::test]
async fn model_credentials_resolve_only_from_the_selected_vaults() -> Result<()> {
    let harness = BasicExoHarness::in_memory(BasicExoHarnessConfig {
        root: Default::default(),
        secret_backend: SecretBackendChoice::Static([1; 32]),
        sandbox_default: SandboxProvider::LocalProcess,
        sandbox_policy: None,
        sandbox_backends: vec![SandboxBackendRegistration::local_process()],
    })
    .await?;
    let runtime = exoharness::vault::global_vault(&harness).await?;
    let user = harness.create_vault("alice").await?;
    runtime
        .put_secret(PutSecretRequest {
            name: "provider".into(),
            policy: Some(
                exoharness::CredentialDestination::origin("https://api.openai.com")
                    .unwrap()
                    .into(),
            ),
            secret: Secret::Key {
                value: "runtime-key".into(),
            },
        })
        .await?;
    let user_secret = user
        .put_secret(PutSecretRequest {
            name: "provider".into(),
            policy: Some(
                exoharness::CredentialDestination::origin("https://api.openai.com")
                    .unwrap()
                    .into(),
            ),
            secret: Secret::Key {
                value: "user-key".into(),
            },
        })
        .await?;
    let agent = harness
        .new_agent(NewAgentRequest {
            vaults: vec![],
            name: "agent".into(),
            slug: "agent".into(),
        })
        .await?;
    let thread = agent
        .new_thread(NewThreadRequest {
            vaults: vec![user.record().id],
            ..Default::default()
        })
        .await?;
    let definition = exo_managed_agents::AgentDefinition::parse(
        "---\nharness: basic\nconfig:\n  model: gpt-5.6-sol\n  credential: provider\n---\nTest."
            .into(),
    )?;
    let mut config = crate::managed_agents::agent_config(
        &definition,
        SandboxProvider::LocalProcess,
        None,
        None,
    )?;
    let model = resolve_model(thread.as_ref(), &config).await?;
    assert_eq!(model.api_key, "user-key");
    user.update_secret(
        &user_secret,
        Secret::Key {
            value: "rotated-user-key".into(),
        }
        .into(),
    )
    .await?;
    assert_eq!(
        resolve_model(thread.as_ref(), &config).await?.api_key,
        "rotated-user-key"
    );
    let global_thread = agent.new_thread(Default::default()).await?;
    assert_eq!(
        resolve_model(global_thread.as_ref(), &config)
            .await?
            .api_key,
        "runtime-key"
    );
    config.credential = Some(user_secret.to_string());
    assert!(
        resolve_model(global_thread.as_ref(), &config)
            .await
            .is_err()
    );
    config.credential = None;
    assert!(
        resolve_model(thread.as_ref(), &config)
            .await
            .unwrap_err()
            .to_string()
            .contains("config.credential")
    );
    Ok(())
}
