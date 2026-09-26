use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock};

use crate::{AgentConfig, ConversationConfig};
use exoharness::{
    ConversationHandle, CreateSandboxRequest, DEFAULT_SANDBOX_IMAGE, EventData, EventKind,
    EventQuery, EventQueryDirection, FileSystemMount, FileSystemMountMode, Result, SandboxProvider,
};
use futures::{StreamExt, TryStreamExt};
use tokio::sync::Mutex as AsyncMutex;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConversationSandboxInfo {
    pub(crate) id: String,
    pub(crate) policy: Option<exoharness::EgressPolicy>,
    pub(crate) provider: SandboxProvider,
    pub(crate) image: String,
    pub(crate) default_workdir: String,
    pub(crate) file_system_mounts: Vec<FileSystemMount>,
    pub(crate) durable_file_systems: Vec<exoharness::DurableFileSystem>,
    pub(crate) enable_networking: bool,
    pub(crate) idle_seconds: u64,
}

impl ConversationSandboxInfo {
    pub(crate) fn matches_spec(&self, spec: &ConversationSandboxSpec) -> bool {
        self.provider == spec.provider
            && (spec.image.is_empty() || self.image == spec.image)
            && self.default_workdir == spec.default_workdir
            && self.file_system_mounts == spec.file_system_mounts
            && self.durable_file_systems == spec.durable_file_systems
            && self.enable_networking == spec.enable_networking
            && self.idle_seconds == spec.idle_seconds
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConversationSandboxSpec {
    pub(crate) provider: SandboxProvider,
    pub(crate) image: String,
    pub(crate) default_workdir: String,
    pub(crate) file_system_mounts: Vec<FileSystemMount>,
    pub(crate) durable_file_systems: Vec<exoharness::DurableFileSystem>,
    pub(crate) enable_networking: bool,
    pub(crate) idle_seconds: u64,
}

pub(crate) async fn ensure_conversation_sandbox(
    conversation: &dyn ConversationHandle,
    agent_config: &AgentConfig,
    config: &ConversationConfig,
    healthcheck_program: Option<&str>,
) -> Result<String> {
    let sandbox_lock = conversation_sandbox_lock(&conversation.record().id.to_string());
    let _guard = sandbox_lock.lock().await;
    let spec = conversation_sandbox_spec(agent_config, config);
    let policy = sandbox_policy(conversation, agent_config, config).await?;

    // Of the still-active candidates in conversation history, prefer the most recent one
    // that was either explicitly attached or matches the spec derived from configuration.
    for candidate in conversation_sandbox_candidates(conversation)
        .await?
        .into_iter()
        .rev()
    {
        match candidate {
            ConversationSandboxCandidate::Attached { id } => {
                anyhow::ensure!(
                    config.resources.is_empty(),
                    "filesystem resources require an Exo-managed sandbox"
                );
                anyhow::ensure!(
                    policy
                        .as_ref()
                        .is_none_or(|policy| policy.credentials.is_empty()),
                    "sandbox credentials require an Exo-managed sandbox"
                );
                return Ok(id);
            }
            ConversationSandboxCandidate::Created(sandbox)
                if sandbox.matches_spec(&spec)
                    && matches_sandbox_policy(sandbox.policy.as_ref(), policy.as_ref()) =>
            {
                if let Some(program) = healthcheck_program {
                    let healthcheck = conversation
                        .run_in_sandbox(exoharness::RunInSandboxRequest {
                            id: sandbox.id.clone(),
                            command: vec![program.to_owned(), "-lc".to_owned(), "true".to_owned()],
                            env: Default::default(),
                        })
                        .await;
                    if healthcheck.is_err() {
                        continue;
                    }
                }
                return Ok(sandbox.id);
            }
            ConversationSandboxCandidate::Created(_) => {}
        }
    }

    create_sandbox(conversation, config, spec, policy).await
}

pub async fn attached_conversation_sandbox(
    conversation: &dyn ConversationHandle,
) -> Result<Option<String>> {
    Ok(
        match conversation_sandbox_candidates(conversation)
            .await?
            .into_iter()
            .next_back()
        {
            Some(ConversationSandboxCandidate::Attached { id }) => Some(id),
            Some(ConversationSandboxCandidate::Created(_)) | None => None,
        },
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ConversationSandboxCandidate {
    Created(Box<ConversationSandboxInfo>),
    Attached { id: String },
}

impl ConversationSandboxCandidate {
    fn id(&self) -> &str {
        match self {
            Self::Created(sandbox) => &sandbox.id,
            Self::Attached { id } => id,
        }
    }
}

// Replay sandbox lifecycle events and return active candidates in chronological order.
// Stopped or detached sandboxes are excluded; a later start reactivates a stopped sandbox.
async fn conversation_sandbox_candidates(
    conversation: &dyn ConversationHandle,
) -> Result<Vec<ConversationSandboxCandidate>> {
    let events = conversation
        .get_events(Some(EventQuery {
            cursor: None,
            direction: Some(EventQueryDirection::Asc),
            limit: None,
            session_id: None,
            turn_id: None,
            types: Some(vec![
                EventKind::SANDBOX_CREATED,
                EventKind::SANDBOX_STARTED,
                EventKind::SANDBOX_STOPPED,
                EventKind::SANDBOX_ATTACHED,
                EventKind::SANDBOX_DETACHED,
            ]),
        }))
        .await?
        .events;
    let mut candidates = Vec::new();
    let mut inactive = HashSet::new();
    for event in events {
        match event.data {
            EventData::SandboxCreated {
                sandbox_id,
                provider,
                image,
                default_workdir,
                file_system_mounts,
                durable_file_systems,
                enable_networking,
                idle_seconds,
                policy,
                ..
            } => {
                candidates.push(ConversationSandboxCandidate::Created(Box::new(
                    ConversationSandboxInfo {
                        id: sandbox_id,
                        policy,
                        provider,
                        image,
                        default_workdir,
                        file_system_mounts,
                        durable_file_systems,
                        enable_networking,
                        idle_seconds,
                    },
                )));
            }
            EventData::SandboxAttached { sandbox_id, .. } => {
                candidates.push(ConversationSandboxCandidate::Attached { id: sandbox_id });
            }
            EventData::SandboxStarted { sandbox_id, .. } => {
                inactive.remove(&sandbox_id);
            }
            EventData::SandboxStopped { sandbox_id }
            | EventData::SandboxDetached { sandbox_id, .. } => {
                inactive.insert(sandbox_id);
            }
            _ => {}
        }
    }
    candidates.retain(|candidate| !inactive.contains(candidate.id()));
    if conversation.caller().is_some() {
        let owned = conversation.list_sandboxes().await?;
        candidates.retain(|candidate| owned.iter().any(|sandbox| sandbox.id == candidate.id()));
    }
    Ok(candidates)
}

pub(crate) async fn create_conversation_sandbox(
    conversation: &dyn ConversationHandle,
    agent_config: &AgentConfig,
    config: &ConversationConfig,
) -> Result<String> {
    let spec = conversation_sandbox_spec(agent_config, config);
    let policy = sandbox_policy(conversation, agent_config, config).await?;
    create_sandbox(conversation, config, spec, policy).await
}

async fn create_sandbox(
    conversation: &dyn ConversationHandle,
    config: &ConversationConfig,
    spec: ConversationSandboxSpec,
    policy: Option<exoharness::EgressPolicy>,
) -> Result<String> {
    conversation
        .create_sandbox(CreateSandboxRequest {
            name: config
                .environment
                .as_ref()
                .and_then(|env| env.config.name.clone()),
            provider: spec.provider,
            image: spec.image,
            resources: config
                .environment
                .as_ref()
                .and_then(|env| env.config.resources),
            default_workdir: Some(spec.default_workdir),
            file_system_mounts: Some(spec.file_system_mounts),
            durable_file_systems: Some(spec.durable_file_systems),
            policy,
            enable_networking: Some(spec.enable_networking),
            idle_seconds: Some(spec.idle_seconds),
        })
        .await
}

pub(crate) async fn conversation_sandboxes(
    conversation: &dyn ConversationHandle,
) -> Result<Vec<ConversationSandboxInfo>> {
    Ok(conversation_sandbox_candidates(conversation)
        .await?
        .into_iter()
        .filter_map(|candidate| match candidate {
            ConversationSandboxCandidate::Created(sandbox) => Some(*sandbox),
            ConversationSandboxCandidate::Attached { .. } => None,
        })
        .collect())
}

// The agent-scoped sandbox is shared by every conversation, so its spec must
// not depend on which conversation asks for it: it is derived from the agent
// config alone.
pub(crate) fn agent_sandbox_spec(agent_config: &AgentConfig) -> ConversationSandboxSpec {
    ConversationSandboxSpec {
        provider: agent_config.sandbox.provider.clone(),
        image: agent_config
            .sandbox
            .image
            .clone()
            .unwrap_or_else(|| DEFAULT_SANDBOX_IMAGE.to_string()),
        default_workdir: agent_config
            .sandbox
            .mounts
            .first()
            .map(|mount| mount.mount_path.clone())
            .unwrap_or_else(|| "/".to_string()),
        file_system_mounts: normalize_mounts(&agent_config.sandbox.mounts),
        durable_file_systems: Vec::new(),
        enable_networking: agent_config.sandbox.enable_networking,
        idle_seconds: 300,
    }
}

pub(crate) fn conversation_sandbox_spec(
    agent_config: &AgentConfig,
    config: &ConversationConfig,
) -> ConversationSandboxSpec {
    let environment = config.environment.as_ref().map(|env| &env.config);
    ConversationSandboxSpec {
        provider: config.effective_sandbox_provider(agent_config),
        image: config
            .effective_sandbox_image(agent_config)
            .map(str::to_string)
            .unwrap_or_default(),
        default_workdir: environment
            .and_then(|env| env.default_workdir.clone())
            .or_else(|| (!config.resources.is_empty()).then(|| "/workspace".to_string()))
            .or_else(|| {
                config
                    .mounts
                    .first()
                    .map(|mount| mount.mount_path.clone())
                    .or_else(|| {
                        config
                            .durable_file_systems
                            .first()
                            .map(|file_system| file_system.mount_path.clone())
                    })
            })
            .unwrap_or_else(|| "/".to_string()),
        file_system_mounts: normalize_mounts(&config.mounts)
            .into_iter()
            .chain(config.resource_mounts.clone())
            .collect(),
        durable_file_systems: config.durable_file_systems.clone(),
        enable_networking: environment
            .and_then(|env| {
                env.policy
                    .as_ref()
                    .map(|policy| policy.networking_enabled())
                    .or(env.enable_networking)
            })
            .unwrap_or(agent_config.sandbox.enable_networking),
        idle_seconds: environment.and_then(|env| env.idle_seconds).unwrap_or(300),
    }
}

async fn sandbox_policy(
    conversation: &dyn ConversationHandle,
    agent_config: &AgentConfig,
    config: &ConversationConfig,
) -> Result<Option<exoharness::EgressPolicy>> {
    let mut configured = config
        .environment
        .as_ref()
        .and_then(|env| env.config.policy.clone());
    if config.resources.iter().any(|resource| {
        matches!(
            resource.definition.source,
            exoharness::resources::ResourceSource::GitRepository { .. }
        )
    }) {
        configured.get_or_insert_with(|| {
            if conversation_sandbox_spec(agent_config, config).enable_networking {
                exoharness::SandboxNetworkPolicy::Unrestricted.into()
            } else {
                exoharness::SandboxNetworkPolicy::Disabled.into()
            }
        });
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
        let policy = configured.as_mut().expect("Git resource policy");
        anyhow::ensure!(
            policy.networking_enabled(),
            "Git credentials require sandbox networking"
        );
        if let exoharness::SandboxNetworkPolicy::Limited { allowed_hosts } = &policy.networking {
            anyhow::ensure!(
                allowed_hosts
                    .iter()
                    .any(|allowed| allowed.eq_ignore_ascii_case(host)),
                "Git resource host {host} is not allowed by the environment network policy"
            );
        }
        if host == "github.com" {
            if let exoharness::SandboxNetworkPolicy::Limited { allowed_hosts } = &policy.networking
            {
                anyhow::ensure!(
                    allowed_hosts
                        .iter()
                        .any(|host| host.eq_ignore_ascii_case("api.github.com")),
                    "GitHub API access requires api.github.com in the environment network policy"
                );
            }
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
                policy
                    .credentials
                    .push(exoharness::EgressCredentialBinding {
                        name: reference.secret_id.to_string(),

                        environment_variable: "GH_TOKEN".into(),
                        networking: exoharness::CredentialNetworkPolicy::Limited {
                            allowed_hosts: vec!["api.github.com".into()],
                        },
                        injection_location: exoharness::CredentialInjectionLocation {
                            header: true,
                        },
                    });
            }
        }
        policy
            .credentials
            .push(exoharness::EgressCredentialBinding {
                name: reference.secret_id.to_string(),

                environment_variable: resource.definition.git_credential_variable(),
                networking: exoharness::CredentialNetworkPolicy::Limited {
                    allowed_hosts: vec![host.to_owned()],
                },
                injection_location: exoharness::CredentialInjectionLocation { header: true },
            });
    }
    let module = agent_config
        .typescript
        .as_ref()
        .and_then(|config| std::path::Path::new(&config.module_path).file_name())
        .and_then(|name| name.to_str());
    let variable = match module {
        Some("codex-harness.ts") => Some("OPENAI_API_KEY"),
        Some("claude-code-harness.ts") => Some("ANTHROPIC_API_KEY"),
        Some("pi-harness.ts") => Some(
            match agent_config
                .model
                .split_once('/')
                .map(|(provider, _)| provider)
                .unwrap_or("openai")
            {
                "openai" => "OPENAI_API_KEY",
                "anthropic" => "ANTHROPIC_API_KEY",
                "google" => "GEMINI_API_KEY",
                provider => anyhow::bail!(
                    "Pi API-key credentials are not configured for provider {provider}"
                ),
            },
        ),
        _ => None,
    };
    if let Some(variable) = variable {
        let reference =
            crate::harness_helpers::model_credential(conversation, agent_config).await?;
        let endpoint =
            exoharness::vault::model_endpoint(agent_config.base_url.as_deref(), variable)?;
        let host = endpoint.host_str().expect("validated model endpoint");
        let vault = exoharness::vault::require_vault(conversation, &reference.vault_id).await?;
        let target =
            exoharness::vault::SecretTarget::http(&endpoint.origin().ascii_serialization())?;
        let metadata = vault
            .list_secrets()
            .await?
            .into_iter()
            .find(|secret| secret.id == reference.secret_id)
            .ok_or_else(|| anyhow::anyhow!("model credential is unavailable"))?;
        anyhow::ensure!(
            metadata.r#type == exoharness::SecretType::Key,
            "model credentials must be API keys"
        );
        anyhow::ensure!(
            metadata.target.as_ref() == Some(&target),
            "model credential {} is not authorized for {}; add its destination with `exo vault secret update <vault> {} --http-origin {}`",
            metadata.name,
            endpoint.origin().ascii_serialization(),
            metadata.name,
            endpoint.origin().ascii_serialization()
        );
        let policy = configured.get_or_insert_with(|| {
            if conversation_sandbox_spec(agent_config, config).enable_networking {
                exoharness::SandboxNetworkPolicy::Unrestricted.into()
            } else {
                exoharness::SandboxNetworkPolicy::Disabled.into()
            }
        });
        anyhow::ensure!(
            policy.networking_enabled(),
            "sandbox models require networking"
        );
        if let exoharness::SandboxNetworkPolicy::Limited { allowed_hosts } = &policy.networking {
            anyhow::ensure!(
                allowed_hosts
                    .iter()
                    .any(|allowed| allowed.eq_ignore_ascii_case(host)),
                "model endpoint {host} is not allowed by the environment network policy"
            );
        }
        if let Some(existing) = policy
            .credentials
            .iter_mut()
            .find(|binding| binding.environment_variable == variable)
        {
            let selected = exoharness::vault::find_secret(conversation, &existing.name).await?;
            let exoharness::CredentialNetworkPolicy::Limited { allowed_hosts } =
                &existing.networking;
            anyhow::ensure!(
                selected.as_ref() == Some(&reference)
                    && existing.injection_location.header
                    && allowed_hosts
                        .iter()
                        .any(|allowed| allowed.eq_ignore_ascii_case(host)),
                "environment credential {variable} conflicts with the model credential"
            );
            existing.name = reference.secret_id.to_string();
        } else {
            policy
                .credentials
                .push(exoharness::EgressCredentialBinding {
                    name: reference.secret_id.to_string(),
                    environment_variable: variable.into(),
                    networking: exoharness::CredentialNetworkPolicy::Limited {
                        allowed_hosts: vec![host.to_owned()],
                    },
                    injection_location: exoharness::CredentialInjectionLocation { header: true },
                });
        }
    }
    if !matches!(module, Some("codex-harness.ts" | "claude-code-harness.ts")) {
        return Ok(configured);
    }
    let Some(selection) = exo_managed_agents::vaults::load_selection(conversation).await? else {
        return Ok(configured);
    };
    if selection.bindings.is_empty() {
        return Ok(configured);
    }
    let mut policy = configured.unwrap_or_else(|| {
        if conversation_sandbox_spec(agent_config, config).enable_networking {
            exoharness::SandboxNetworkPolicy::Unrestricted.into()
        } else {
            exoharness::SandboxNetworkPolicy::Disabled.into()
        }
    });
    anyhow::ensure!(
        policy.networking_enabled(),
        "native MCP requires sandbox networking"
    );
    for selected in selection.bindings {
        let exoharness::vault::SecretTarget::Mcp { server_url } = selected.target else {
            anyhow::bail!("expected MCP destination");
        };
        let endpoint = url::Url::parse(&server_url)?;
        let host = endpoint
            .host_str()
            .ok_or_else(|| anyhow::anyhow!("MCP endpoint has no host"))?;
        if let exoharness::SandboxNetworkPolicy::Limited { allowed_hosts } = &policy.networking {
            anyhow::ensure!(
                allowed_hosts
                    .iter()
                    .any(|allowed| allowed.eq_ignore_ascii_case(host)),
                "MCP endpoint {host} is not allowed by the environment network policy"
            );
        }
        let Some(secret) = selected.secret else {
            continue;
        };
        anyhow::ensure!(
            endpoint.scheme() == "https" && endpoint.port_or_known_default() == Some(443),
            "sandbox MCP credentials require HTTPS on port 443"
        );
        let variable = crate::mcp::credential_variable(&secret.secret_id);
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
        policy
            .credentials
            .push(exoharness::EgressCredentialBinding {
                name: secret.secret_id.to_string(),

                environment_variable: variable,
                networking: exoharness::CredentialNetworkPolicy::Limited {
                    allowed_hosts: vec![host.to_owned()],
                },
                injection_location: exoharness::CredentialInjectionLocation { header: true },
            });
    }
    Ok(Some(policy))
}

fn normalize_mounts(mounts: &[FileSystemMount]) -> Vec<FileSystemMount> {
    mounts
        .iter()
        .map(|mount| FileSystemMount {
            host_path: mount.host_path.clone(),
            mount_path: mount.mount_path.clone(),
            mode: match mount.mode {
                FileSystemMountMode::ReadOnly => FileSystemMountMode::ReadOnly,
                FileSystemMountMode::ReadWrite => FileSystemMountMode::ReadWrite,
            },
            internal: Some(mount.internal.unwrap_or(false)),
        })
        .collect()
}

fn conversation_sandbox_lock(conversation_id: &str) -> Arc<AsyncMutex<()>> {
    static LOCKS: OnceLock<Mutex<HashMap<String, Arc<AsyncMutex<()>>>>> = OnceLock::new();
    let locks = LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut locks = locks
        .lock()
        .expect("conversation sandbox lock registry poisoned");
    Arc::clone(
        locks
            .entry(conversation_id.to_string())
            .or_insert_with(|| Arc::new(AsyncMutex::new(()))),
    )
}

fn matches_sandbox_policy(
    actual: Option<&exoharness::EgressPolicy>,
    configured: Option<&exoharness::EgressPolicy>,
) -> bool {
    configured.is_none_or(|configured| actual == Some(configured))
}

#[cfg(test)]
mod tests {
    use super::*;
    use exoharness::{CredentialNetworkPolicy, SandboxNetworkPolicy};

    #[tokio::test]
    async fn native_model_credentials_use_selected_vaults_and_ordinary_egress_policy() -> Result<()>
    {
        use exoharness::{ExoHarness, NewAgentRequest, PutSecretRequest, Secret};
        let temp = tempfile::tempdir()?;
        let harness =
            exoharness::BasicExoHarness::new(crate::test_support::local_test_config(temp.path()))
                .await?;
        let vault = harness.create_vault("personal").await?;
        let agent = harness
            .new_agent(NewAgentRequest {
                slug: "test".into(),
                name: "test".into(),
                vaults: vec![],
            })
            .await?;
        let unattached = agent.new_thread(Default::default()).await?;
        let thread = unattached.attach_vaults(vec![vault.record().id]).await?;
        for (harness_name, model, variable, host) in [
            ("codex", "gpt-5.6-sol", "OPENAI_API_KEY", "api.openai.com"),
            (
                "claude-code",
                "claude-sonnet-4-6",
                "ANTHROPIC_API_KEY",
                "api.anthropic.com",
            ),
            (
                "pi",
                "google/gemini-test",
                "GEMINI_API_KEY",
                "generativelanguage.googleapis.com",
            ),
        ] {
            let definition = exo_managed_agents::AgentDefinition::parse(format!(
                "---\nname: test\nharness: {harness_name}\nconfig:\n  model: {model}\n  credential: {harness_name}\n---\nHelp."
            ))?;
            let mut agent_config = crate::managed_agents::agent_config(
                &definition,
                SandboxProvider::Firecracker,
                None,
                None,
            )?;
            agent_config.sandbox.enable_networking = true;
            let secret = vault
                .put_secret(PutSecretRequest {
                    name: harness_name.into(),
                    target: None,
                    secret: Secret::Key {
                        value: "vault-key".into(),
                    },
                })
                .await?;
            let config = ConversationConfig::default();
            assert!(
                sandbox_policy(unattached.as_ref(), &agent_config, &config)
                    .await
                    .is_err()
            );
            assert!(
                sandbox_policy(thread.as_ref(), &agent_config, &config)
                    .await
                    .is_err()
            );
            vault
                .update_secret(
                    &secret,
                    exoharness::UpdateSecretRequest {
                        secret: None,
                        target: Some(exoharness::vault::SecretTarget::http(&format!(
                            "https://{host}"
                        ))?),
                    },
                )
                .await?;
            let policy = sandbox_policy(thread.as_ref(), &agent_config, &config)
                .await?
                .unwrap();
            assert_eq!(policy.credentials.len(), 1);
            let binding = &policy.credentials[0];
            assert_eq!(binding.name, secret.to_string());
            assert_eq!(binding.environment_variable, variable);
            assert_eq!(
                binding.networking,
                CredentialNetworkPolicy::Limited {
                    allowed_hosts: vec![host.into()]
                }
            );
            assert!(binding.injection_location.header);
            agent_config.base_url = Some("https://different.example/v1".into());
            assert!(
                sandbox_policy(thread.as_ref(), &agent_config, &config)
                    .await
                    .is_err()
            );
            vault.delete_secret(&secret).await?;
        }
        Ok(())
    }

    #[tokio::test]
    async fn git_credentials_require_an_attached_vault_and_environment_access() -> Result<()> {
        use exoharness::{ExoHarness, NewAgentRequest, NewThreadRequest, PutSecretRequest, Secret};
        let temp = tempfile::tempdir()?;
        let harness =
            exoharness::BasicExoHarness::new(crate::test_support::local_test_config(temp.path()))
                .await?;
        let vault = harness.create_vault("personal").await?;
        let secret = vault
            .put_secret(PutSecretRequest {
                name: "github-git".into(),
                target: Some(exoharness::vault::SecretTarget::http("https://github.com")?),
                secret: Secret::Key {
                    value: "test-token".into(),
                },
            })
            .await?;
        let agent = harness
            .new_agent(NewAgentRequest {
                slug: "test".into(),
                name: "test".into(),
                vaults: vec![],
            })
            .await?;
        let thread = agent.new_thread(NewThreadRequest::default()).await?;
        let definition = exo_managed_agents::AgentDefinition::parse(
            "---\nname: test\nharness: basic\nconfig:\n  model: test\n---\nUse tools.".into(),
        )?;
        let mut agent_config =
            crate::managed_agents::agent_config(&definition, SandboxProvider::Docker, None, None)?;
        agent_config.sandbox.enable_networking = true;
        let resource = exoharness::resources::PreparedResource {
            definition: exoharness::resources::ResourceDefinition {
                name: "code".into(),
                mount_path: "/workspace".into(),
                mode: FileSystemMountMode::ReadWrite,
                source: exoharness::resources::ResourceSource::GitRepository {
                    path: None,
                    url: Some("https://github.com/org/repo".into()),
                    checkout: None,
                    credential: Some("github-git".into()),
                },
            },
            snapshot: None,
        };
        let mut config = ConversationConfig {
            resources: vec![resource],
            ..Default::default()
        };
        let error = sandbox_policy(thread.as_ref(), &agent_config, &config)
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("not in the selected vaults"),
            "{error}"
        );
        let thread = thread.attach_vaults(vec![vault.record().id]).await?;
        let granted = sandbox_policy(thread.as_ref(), &agent_config, &config)
            .await?
            .unwrap();
        assert_eq!(granted.credentials.len(), 2);
        assert_eq!(granted.credentials[0].name, secret.to_string());
        assert_eq!(granted.credentials[0].environment_variable, "GH_TOKEN");
        assert_eq!(
            granted.credentials[0].networking,
            CredentialNetworkPolicy::Limited {
                allowed_hosts: vec!["api.github.com".into()]
            }
        );
        assert_eq!(granted.credentials[1].name, secret.to_string());
        assert_eq!(
            granted.credentials[1].environment_variable,
            config.resources[0].definition.git_credential_variable()
        );
        assert_eq!(
            granted.credentials[1].networking,
            CredentialNetworkPolicy::Limited {
                allowed_hosts: vec!["github.com".into()]
            }
        );
        config.environment = Some(exoharness::EnvironmentDefinition {
            name: "restricted".into(),
            config: exoharness::CreateSandboxRequest {
                provider: SandboxProvider::Docker,
                image: "test".into(),

                name: None,
                resources: None,
                default_workdir: None,
                file_system_mounts: None,
                durable_file_systems: None,
                policy: Some(
                    SandboxNetworkPolicy::Limited {
                        allowed_hosts: vec!["example.com".into()],
                    }
                    .into(),
                ),
                enable_networking: None,
                idle_seconds: None,
            },
        });
        assert!(
            sandbox_policy(thread.as_ref(), &agent_config, &config)
                .await
                .unwrap_err()
                .to_string()
                .contains("not allowed")
        );
        let policy = config
            .environment
            .as_mut()
            .unwrap()
            .config
            .policy
            .as_mut()
            .unwrap();
        policy.networking = SandboxNetworkPolicy::Limited {
            allowed_hosts: vec!["github.com".into()],
        };
        assert!(
            sandbox_policy(thread.as_ref(), &agent_config, &config)
                .await
                .unwrap_err()
                .to_string()
                .contains("api.github.com")
        );
        config
            .environment
            .as_mut()
            .unwrap()
            .config
            .policy
            .as_mut()
            .unwrap()
            .networking = SandboxNetworkPolicy::Limited {
            allowed_hosts: vec!["github.com".into(), "api.github.com".into()],
        };
        sandbox_policy(thread.as_ref(), &agent_config, &config).await?;
        config.environment = None;
        agent_config.sandbox.enable_networking = false;
        assert!(
            sandbox_policy(thread.as_ref(), &agent_config, &config)
                .await
                .unwrap_err()
                .to_string()
                .contains("networking")
        );
        agent_config.sandbox.enable_networking = true;
        let exoharness::resources::ResourceSource::GitRepository { credential, .. } =
            &mut config.resources[0].definition.source
        else {
            unreachable!()
        };
        *credential = None;
        let removed = sandbox_policy(thread.as_ref(), &agent_config, &config).await?;
        assert!(removed.as_ref().unwrap().credentials.is_empty());
        assert!(!matches_sandbox_policy(Some(&granted), removed.as_ref()));
        Ok(())
    }
}
