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
    let model = sandbox_model_binding(conversation, agent_config).await?;
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
                    model.is_none(),
                    "sandbox model credentials require an Exo-managed sandbox"
                );
                return Ok(id);
            }
            ConversationSandboxCandidate::Created(sandbox)
                if sandbox.matches_spec(&spec)
                    && matches_sandbox_policy(
                        sandbox.policy.as_ref(),
                        policy.as_ref(),
                        model.as_ref(),
                    ) =>
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

    create_sandbox(conversation, config, spec, model, policy).await
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
    let model = sandbox_model_binding(conversation, agent_config).await?;
    let policy = sandbox_policy(conversation, agent_config, config).await?;
    create_sandbox(conversation, config, spec, model, policy).await
}

async fn create_sandbox(
    conversation: &dyn ConversationHandle,
    config: &ConversationConfig,
    spec: ConversationSandboxSpec,
    model: Option<exoharness::SandboxModelBinding>,
    policy: Option<exoharness::EgressPolicy>,
) -> Result<String> {
    conversation
        .create_sandbox(CreateSandboxRequest {
            model,
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
                        model: None,
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
                model: None,
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
                model: None,
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
    model: Option<&exoharness::SandboxModelBinding>,
) -> bool {
    let Some(actual) = actual else {
        return configured.is_none() && model.is_none();
    };
    let credential = model.and_then(|model| {
        actual.credentials.iter().find(|credential| {
            credential.model == Some(model.id)
                && credential.environment_variable == model.environment_variable
        })
    });
    if model.is_some() && credential.is_none() {
        return false;
    }
    let Some(configured) = configured else {
        return true;
    };
    let mut expected = configured.clone();
    if let Some(credential) = credential {
        if let Some(existing) = expected
            .credentials
            .iter_mut()
            .find(|existing| existing.environment_variable == credential.environment_variable)
        {
            if existing.model.is_none() {
                existing.model = credential.model;
            }
        } else {
            expected.credentials.push(credential.clone());
        }
    }
    actual == &expected
}

async fn sandbox_model_binding(
    conversation: &dyn ConversationHandle,
    config: &AgentConfig,
) -> Result<Option<exoharness::SandboxModelBinding>> {
    let module = config
        .typescript
        .as_ref()
        .and_then(|config| std::path::Path::new(&config.module_path).file_name())
        .and_then(|name| name.to_str());
    if !matches!(
        module,
        Some("codex-harness.ts" | "claude-code-harness.ts" | "pi-harness.ts")
    ) {
        return Ok(None);
    }
    let metadata = conversation
        .list_bindings()
        .await?
        .into_iter()
        .filter(|binding| {
            binding.r#type == exoharness::BindingType::Llm && binding.name == config.model
        })
        .max_by_key(|binding| binding.created_at)
        .ok_or_else(|| anyhow::anyhow!("model is not registered: {}", config.model))?;
    let binding = conversation
        .get_binding(&metadata.id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("model binding is unavailable"))?;
    let exoharness::Binding::Llm { model, .. } = binding else {
        anyhow::bail!("sandbox model credential requires an LLM binding");
    };
    let environment_variable = match module {
        Some("codex-harness.ts") => "OPENAI_API_KEY",
        Some("claude-code-harness.ts") => "ANTHROPIC_API_KEY",
        Some("pi-harness.ts") => match model
            .split_once('/')
            .map(|(provider, _)| provider)
            .unwrap_or("openai")
        {
            "openai" => "OPENAI_API_KEY",
            "anthropic" => "ANTHROPIC_API_KEY",
            "google" => "GEMINI_API_KEY",
            provider => {
                anyhow::bail!("Pi API-key bindings are not configured for provider {provider}")
            }
        },
        _ => unreachable!(),
    };
    Ok(Some(exoharness::SandboxModelBinding {
        id: metadata.id,
        environment_variable: environment_variable.into(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use exoharness::{
        CredentialInjectionLocation, CredentialNetworkPolicy, EgressCredentialBinding,
        EgressPolicy, SandboxModelBinding, SandboxNetworkPolicy, Uuid7,
    };

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
                model: None,
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
        assert!(!matches_sandbox_policy(
            Some(&granted),
            removed.as_ref(),
            None
        ));
        Ok(())
    }

    #[test]
    fn saved_sandbox_must_match_the_environment_and_model_grant() {
        let model = SandboxModelBinding {
            id: Uuid7::now(),
            environment_variable: "OPENAI_API_KEY".into(),
        };
        let configured = EgressPolicy::from(SandboxNetworkPolicy::Unrestricted);
        let mut actual = configured.clone();
        actual.credentials.push(EgressCredentialBinding {
            name: format!("model:{}", model.id),
            model: Some(model.id),
            environment_variable: model.environment_variable.clone(),
            networking: CredentialNetworkPolicy::Limited {
                allowed_hosts: vec!["api.openai.com".into()],
            },
            injection_location: CredentialInjectionLocation { header: true },
        });
        assert!(matches_sandbox_policy(
            Some(&actual),
            Some(&configured),
            Some(&model)
        ));
        assert!(!matches_sandbox_policy(
            Some(&configured),
            Some(&configured),
            Some(&model)
        ));
        let mut changed = configured.clone();
        changed.networking = SandboxNetworkPolicy::Limited {
            allowed_hosts: vec!["api.openai.com".into()],
        };
        assert!(!matches_sandbox_policy(
            Some(&actual),
            Some(&changed),
            Some(&model)
        ));
        let mut explicit = actual.clone();
        explicit.credentials[0].model = None;
        assert!(matches_sandbox_policy(
            Some(&actual),
            Some(&explicit),
            Some(&model)
        ));
        explicit.credentials[0].name = "another-key".into();
        assert!(!matches_sandbox_policy(
            Some(&actual),
            Some(&explicit),
            Some(&model)
        ));
        let changed_model = SandboxModelBinding {
            id: Uuid7::now(),
            ..model
        };
        assert!(!matches_sandbox_policy(
            Some(&actual),
            Some(&configured),
            Some(&changed_model)
        ));
    }
}
