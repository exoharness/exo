use crate::{AgentConfig, ConversationConfig};
use exoharness::{ConversationHandle, Result};
use futures::{StreamExt, TryStreamExt};

pub async fn sandbox_policy(
    conversation: &dyn ConversationHandle,
    agent_config: &AgentConfig,
    config: &ConversationConfig,
) -> Result<Option<exoharness::EgressPolicy>> {
    let mut configured = config
        .environment
        .as_ref()
        .and_then(|env| env.config.policy.clone())
        .or_else(|| config.egress_policy.clone());
    let default_policy = if config
        .environment
        .as_ref()
        .and_then(|env| {
            env.config
                .policy
                .as_ref()
                .map(|policy| policy.networking_enabled())
                .or(env.config.enable_networking)
        })
        .unwrap_or(agent_config.sandbox.enable_networking)
    {
        exoharness::SandboxNetworkPolicy::Unrestricted.into()
    } else {
        exoharness::SandboxNetworkPolicy::Disabled.into()
    };
    add_git_resource_bindings(conversation, config, &mut configured, &default_policy).await?;
    let module = agent_config
        .typescript
        .as_ref()
        .and_then(|config| std::path::Path::new(&config.module_path).file_name())
        .and_then(|name| name.to_str());
    let variable = crate::managed_agents::config::sandbox_model_credential_variable(agent_config)?;
    if let Some(variable) = variable {
        add_model_binding(
            conversation,
            agent_config,
            variable,
            &mut configured,
            &default_policy,
        )
        .await?;
    }
    if matches!(module, Some("codex-harness.ts" | "claude-code-harness.ts")) {
        add_native_mcp_bindings(conversation, &mut configured, &default_policy).await?;
    }
    Ok(configured)
}

async fn add_git_resource_bindings(
    conversation: &dyn ConversationHandle,
    config: &ConversationConfig,
    configured: &mut Option<exoharness::EgressPolicy>,
    default_policy: &exoharness::EgressPolicy,
) -> Result<()> {
    if config.resources.iter().any(|resource| {
        matches!(
            resource.definition.source,
            exoharness::resources::ResourceSource::GitRepository { .. }
        )
    }) {
        configured.get_or_insert_with(|| default_policy.clone());
    }
    let credentials =
        futures::stream::iter(config.resources.iter().cloned().map(|resource| async move {
            let reference = match &resource.definition.source {
                exoharness::resources::ResourceSource::GitRepository {
                    credential: Some(name),
                    ..
                } => Some(
                    exoharness::vault::find_secret(conversation, name)
                        .await?
                        .ok_or_else(|| {
                            anyhow::anyhow!(
                                "Git resource credential {name} is not in the selected vaults"
                            )
                        })?,
                ),
                _ => None,
            };
            Ok::<_, anyhow::Error>(reference)
        }))
        .buffered(8)
        .try_collect::<Vec<_>>()
        .await?;
    for (resource, reference) in config.resources.iter().zip(credentials) {
        let exoharness::resources::ResourceSource::GitRepository {
            url: Some(url),
            credential: Some(_),
            ..
        } = &resource.definition.source
        else {
            continue;
        };
        let endpoint = url::Url::parse(url)?;
        anyhow::ensure!(
            endpoint.scheme() == "https" && endpoint.port_or_known_default() == Some(443),
            "sandbox Git credentials require HTTPS on port 443"
        );
        let host = endpoint
            .host_str()
            .ok_or_else(|| anyhow::anyhow!("Git resource URL has no host"))?;
        let reference = reference.expect("Git resource credential was resolved");
        let credential_policy =
            exoharness::vault::credential_policy(conversation, &reference).await?;
        let git_policy = credential_policy.for_destination(
            exoharness::CredentialDestination::origin(&endpoint.origin().ascii_serialization())?,
        )?;
        let github_api = exoharness::CredentialDestination::origin("https://api.github.com")?;
        let policy = configured.as_mut().expect("Git resource policy");
        anyhow::ensure!(
            policy.networking_enabled(),
            "Git credentials require sandbox networking"
        );
        ensure_host_allowed(policy, host, "Git resource host")?;
        if host == "github.com" && credential_policy.permits(&github_api) {
            ensure_host_allowed(policy, "api.github.com", "GitHub API host")?;
            if let Some(existing) = policy
                .credentials
                .iter()
                .find(|binding| binding.environment_variable == "GH_TOKEN")
            {
                anyhow::ensure!(
                    existing.name == reference.secret_id.to_string(),
                    "GitHub resources must share a credential for automatic GH_TOKEN selection"
                );
            } else {
                policy.credentials.push(
                    credential_policy
                        .for_destination(github_api)?
                        .binding(reference.secret_id.to_string(), "GH_TOKEN".into()),
                );
            }
        }
        policy.credentials.push(git_policy.binding(
            reference.secret_id.to_string(),
            resource.definition.git_credential_variable(),
        ));
    }
    Ok(())
}

async fn add_model_binding(
    conversation: &dyn ConversationHandle,
    agent_config: &AgentConfig,
    variable: &str,
    configured: &mut Option<exoharness::EgressPolicy>,
    default_policy: &exoharness::EgressPolicy,
) -> Result<()> {
    let reference = crate::harness_helpers::model_credential(conversation, agent_config).await?;
    let endpoint = exoharness::vault::model_endpoint(agent_config.base_url.as_deref(), variable)?;
    let host = endpoint.host_str().expect("validated model endpoint");
    let target =
        exoharness::CredentialDestination::origin(&endpoint.origin().ascii_serialization())?;
    let credential_policy = exoharness::vault::credential_policy(conversation, &reference)
        .await?
        .for_destination(target.clone())?;
    let policy = configured.get_or_insert_with(|| default_policy.clone());
    anyhow::ensure!(
        policy.networking_enabled(),
        "sandbox models require networking"
    );
    ensure_host_allowed(policy, host, "model endpoint")?;
    if let Some(existing) = policy
        .credentials
        .iter_mut()
        .find(|binding| binding.environment_variable == variable)
    {
        let selected = exoharness::vault::find_secret(conversation, &existing.name).await?;
        anyhow::ensure!(
            selected.as_ref() == Some(&reference)
                && existing.injection_location.header
                && existing.networking.permits(&target),
            "environment credential {variable} conflicts with the model credential"
        );
        existing.name = reference.secret_id.to_string();
    } else {
        policy
            .credentials
            .push(credential_policy.binding(reference.secret_id.to_string(), variable.into()));
    }
    Ok(())
}

async fn add_native_mcp_bindings(
    conversation: &dyn ConversationHandle,
    configured: &mut Option<exoharness::EgressPolicy>,
    default_policy: &exoharness::EgressPolicy,
) -> Result<()> {
    let Some(selection) = exo_managed_agents::vaults::load_selection(conversation).await? else {
        return Ok(());
    };
    if selection.bindings.is_empty() {
        return Ok(());
    }
    let policy = configured.get_or_insert_with(|| default_policy.clone());
    anyhow::ensure!(
        policy.networking_enabled(),
        "native MCP requires sandbox networking"
    );
    for selected in selection.bindings {
        let exoharness::CredentialDestination::Url { url: server_url } = selected.target else {
            anyhow::bail!("expected MCP destination");
        };
        let endpoint = url::Url::parse(&server_url)?;
        let host = endpoint
            .host_str()
            .ok_or_else(|| anyhow::anyhow!("MCP endpoint has no host"))?;
        ensure_host_allowed(policy, host, "MCP endpoint")?;
        let Some(secret) = selected.secret else {
            continue;
        };
        anyhow::ensure!(
            endpoint.scheme() == "https" && endpoint.port_or_known_default() == Some(443),
            "sandbox MCP credentials require HTTPS on port 443"
        );
        let variable = crate::mcp_types::credential_variable(&secret.secret_id);
        if let Some(existing) = policy
            .credentials
            .iter()
            .find(|binding| binding.environment_variable == variable)
        {
            anyhow::ensure!(
                existing.name == secret.secret_id.to_string(),
                "environment credential conflicts with MCP server {}",
                selected.server_name
            );
            continue;
        }
        // Native MCP uses one streamable-HTTP endpoint; preserve its path and query scope.
        let credential_policy = exoharness::vault::credential_policy(conversation, &secret)
            .await?
            .for_destination(exoharness::CredentialDestination::url(&server_url)?)?;
        policy
            .credentials
            .push(credential_policy.binding(secret.secret_id.to_string(), variable));
    }
    Ok(())
}

fn ensure_host_allowed(policy: &exoharness::EgressPolicy, host: &str, what: &str) -> Result<()> {
    if let exoharness::SandboxNetworkPolicy::Limited { allowed_hosts } = &policy.networking {
        anyhow::ensure!(
            allowed_hosts
                .iter()
                .any(|allowed| allowed.eq_ignore_ascii_case(host)),
            "{what} {host} is not allowed by the environment network policy"
        );
    }
    Ok(())
}
