mod config;
mod executor;
pub use config::{TypeScriptHarnessPreset, agent_config};

use std::{path::Path, sync::Arc};

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use exo_managed_agents::{self as managed, AgentBackend, AgentDefinition};
use exo_mcp::McpToolSet;
use exoharness::{AgentHandle, ExoHarness, ThreadHandle};

use crate::{AgentConfig, ConversationConfig, ConversationModelConfig, LocalProvider};

#[derive(Default)]
pub struct LocalAgentSetup {
    pub agent: Option<AgentConfig>,
    pub model: Option<String>,
    pub thread: ConversationConfig,
}

#[async_trait]
impl AgentBackend for LocalProvider {
    fn exoharness(&self) -> Arc<dyn ExoHarness> {
        self.state.clone()
    }

    async fn configure_agent(
        &self,
        agent: &Arc<dyn AgentHandle>,
        definition: &AgentDefinition,
    ) -> Result<()> {
        let mut config = match &self.managed.agent {
            Some(config) => config.clone(),
            None => self.executor.agent_config(definition)?,
        };
        config.instructions = vec![crate::harness_helpers::system_message(
            &definition.system_prompt(),
        )];
        let mut resources = definition.frontmatter.resources.clone();
        let base = definition
            .path()
            .and_then(Path::parent)
            .unwrap_or(Path::new("."));
        for resource in &mut resources {
            anyhow::ensure!(
                definition.path().is_some() || resource.local_path().is_none_or(Path::is_absolute),
                "relative resource paths require a local agent file; use a Git URL or an absolute path on the provider"
            );
            resource.resolve_path(base)?;
        }
        config.resources = agent.prepare_resources(resources).await?;
        crate::harness_config::store_agent_config(agent.as_ref(), &config).await
    }

    async fn configure_thread(
        &self,
        agent: &dyn AgentHandle,
        thread: &dyn ThreadHandle,
        created: bool,
    ) -> Result<managed::ThreadInfo> {
        let agent_config = crate::load_agent_config(agent).await?;
        let current_model = crate::get_conversation_model_override(thread).await?;
        let preferred = current_model
            .as_ref()
            .map(|config| config.model.as_str())
            .unwrap_or(&agent_config.model);
        let model = resolve_model(
            self.state.as_ref(),
            self.managed.model.as_deref(),
            preferred,
        )
        .await?;
        let mut config = if created {
            ConversationConfig {
                resources: agent_config.resources.clone(),
                sandbox_image: agent_config.sandbox.image.clone(),
                sandbox_provider: Some(agent_config.sandbox.provider.clone()),
                ..Default::default()
            }
        } else {
            crate::load_conversation_config(thread).await?
        };
        for resource in &mut config.resources {
            if let Some(updated) = agent_config
                .resources
                .iter()
                .find(|candidate| resource.same_workspace(candidate))
            {
                *resource = updated.clone();
            }
        }
        if let Some(definition) = managed::load_definition(agent).await? {
            config.permissions = definition.permissions();
        }
        let requested = &self.managed.thread;
        if let Some(provider) = &requested.sandbox_provider {
            config.sandbox_provider = Some(provider.clone());
        }
        if let Some(image) = &requested.sandbox_image {
            config.sandbox_image = Some(image.clone());
        }
        for mount in &requested.mounts {
            config
                .mounts
                .retain(|other| other.mount_path != mount.mount_path);
            config.mounts.push(mount.clone());
        }
        if let Some(environment) = &thread.record().environment {
            anyhow::ensure!(
                requested.sandbox_provider.is_none()
                    && requested.sandbox_image.is_none()
                    && requested.mounts.is_empty(),
                "an environment fixes the sandbox provider and image; only additional mounts can be set when creating the thread"
            );
            config.environment = Some(environment.clone());
            config.sandbox_provider = Some(environment.config.provider.clone());
            config.sandbox_image = Some(environment.config.image.clone());
            config.mounts = environment
                .config
                .file_system_mounts
                .clone()
                .unwrap_or_default();
            config.durable_file_systems = environment
                .config
                .durable_file_systems
                .clone()
                .unwrap_or_default();
            config.sandbox_scope = Some(crate::SandboxScope::Conversation);
        }
        if !config.resources.is_empty() {
            anyhow::ensure!(
                matches!(
                    config.effective_sandbox_provider(&agent_config).as_str(),
                    "apple_container" | "docker" | "local_process" | "firecracker" | "smolvm"
                ),
                "filesystem resources require Apple Container, Docker, Firecracker, SmolVM or local-process sandboxes"
            );
            for resource in &config.resources {
                for mount_path in config
                    .mounts
                    .iter()
                    .map(|m| &m.mount_path)
                    .chain(config.durable_file_systems.iter().map(|m| &m.mount_path))
                {
                    exoharness::resources::validate_mount_overlap(
                        &resource.definition.mount_path,
                        mount_path,
                    )?;
                }
            }
            config.sandbox_scope = Some(crate::SandboxScope::Conversation);
            config.resource_mounts = thread
                .materialize_resources(
                    config.resources.clone(),
                    config.effective_sandbox_provider(&agent_config),
                )
                .await?;
        }
        let mcp_tools = self
            .executor
            .configure_managed_thread(agent, thread, &agent_config, &config)
            .await?;
        crate::harness_config::store_conversation_config(thread, &config).await?;
        if self.managed.model.is_some() || model != preferred {
            crate::put_conversation_model_override(
                thread,
                Some(ConversationModelConfig {
                    model: model.clone(),
                    max_output_tokens: None,
                }),
            )
            .await?;
        }
        Ok(managed::ThreadInfo {
            model: Some(model),
            mcp_tools: Some(mcp_tools),
        })
    }

    async fn end_session(
        &self,
        thread: &dyn ThreadHandle,
        session_id: exoharness::SessionId,
    ) -> Result<()> {
        thread.end_session(session_id).await
    }
}

impl LocalProvider {
    pub fn managed(
        state: Arc<dyn ExoHarness>,
        config: exoharness::BasicExoHarnessConfig,
        env: std::collections::HashMap<String, String>,
        pricing: Arc<cost::PricingTable>,
    ) -> Result<Self> {
        let executor = executor::ManagedExecutor::new(state.clone(), config, env, pricing)?;
        Ok(Self::new(state, Arc::new(executor)))
    }

    pub fn with_managed_agents(mut self, setup: LocalAgentSetup) -> Self {
        self.managed = setup;
        self
    }
}

async fn resolve_model(
    root: &dyn ExoHarness,
    requested: Option<&str>,
    preferred: &str,
) -> Result<String> {
    let registered: Vec<_> = root
        .list_bindings()
        .await?
        .into_iter()
        .filter_map(|binding| match binding.binding {
            exoharness::Binding::Llm { name, .. } => Some(name),
            _ => None,
        })
        .collect();
    let selected = requested.unwrap_or(preferred);
    if registered.iter().any(|model| model == selected) {
        return Ok(selected.to_string());
    }
    if requested.is_some() {
        bail!(
            "model is not registered: {selected}; register it with `exo model create {selected} --secret <secret>`"
        );
    }
    let model = registered.first().context("no model is registered; run `exo vault secret create global openai --token-env OPENAI_API_KEY` and `exo model create gpt-5.6-sol --secret openai`")?;
    tracing::warn!(
        preferred,
        model,
        "model is not registered; using the only registered model"
    );
    Ok(model.clone())
}

struct PreparedMcp {
    tools: Arc<McpToolSet>,
    selection: Option<managed::vaults::VaultSelection>,
    selection_is_new: bool,
    config: exoharness::BasicExoHarnessConfig,
    servers: Vec<exo_mcp::McpServerConfig>,
}

async fn connect_mcp(
    agent: &dyn AgentHandle,
    thread: &dyn ThreadHandle,
    config: &exoharness::BasicExoHarnessConfig,
) -> Result<PreparedMcp> {
    let vaults = thread.list_vaults().await?;
    let saved = managed::vaults::load_selection(thread).await?;
    let selection_is_new = saved.is_none();
    let servers = match managed::load_definition(agent).await? {
        Some(definition) => definition.resolve_mcp_servers(&()).await?,
        None => Vec::new(),
    };
    let selection = match saved {
        Some(selection) => Some(selection),
        None if servers.is_empty() && vaults.len() == 1 => None,
        None => Some(managed::vaults::VaultSelection::from_vaults(&vaults, &servers).await?),
    };
    let tools = match &selection {
        Some(selection) => {
            selection.validate_servers(&servers)?;
            let credentials = managed::vaults::VaultMcpCredentials::new(vaults, selection.clone());
            Arc::new(McpToolSet::connect_with_provider(&servers, Arc::new(credentials)).await?)
        }
        None => Arc::default(),
    };
    Ok(PreparedMcp {
        tools,
        selection,
        selection_is_new,
        config: config.clone(),
        servers,
    })
}

impl PreparedMcp {
    fn validate_thread(
        &self,
        agent_config: &AgentConfig,
        config: &ConversationConfig,
    ) -> Result<()> {
        let provider = config
            .sandbox_provider
            .as_ref()
            .unwrap_or(&agent_config.sandbox.provider);
        if *provider != exoharness::SandboxProvider::LocalProcess {
            for mount in agent_config
                .sandbox
                .mounts
                .iter()
                .chain(config.mounts.iter())
            {
                self.config
                    .validate_secret_mount(Path::new(&mount.host_path))?;
            }
        } else if let Some(selection) = &self.selection
            && (selection.vaults.len() > 1 || selection.bindings.iter().any(|b| b.secret.is_some()))
        {
            bail!(
                "vault-backed chat requires an isolated sandbox; local-process can read host credentials"
            );
        }
        Ok(())
    }

    async fn configure_thread(
        &self,
        agent_config: &AgentConfig,
        conversation: &dyn ThreadHandle,
        config: &ConversationConfig,
    ) -> Result<()> {
        self.validate_thread(agent_config, config)?;
        if self.selection_is_new
            && let Some(selection) = &self.selection
        {
            selection.save(conversation).await?;
        }

        let inventory = serde_json::to_value(self.tools.tools())?;
        let previous = conversation
            .get_events(Some(exoharness::EventQuery {
                direction: Some(exoharness::EventQueryDirection::Desc),
                limit: Some(1),
                types: Some(vec![exoharness::EventKind::custom("mcp_tools")]),
                ..Default::default()
            }))
            .await?;
        if (!self.tools.tools().is_empty() || !previous.events.is_empty())
            && !matches!(previous.events.first().map(|event| &event.data),
                Some(crate::EventData::Custom { payload, .. }) if payload == &inventory)
        {
            conversation
                .add_events(exoharness::AddEventsRequest {
                    session_id: None,
                    turn_id: None,
                    data: vec![crate::EventData::Custom {
                        event_type: "mcp_tools".to_string(),
                        payload: inventory,
                    }],
                })
                .await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod permission_tests {
    use super::*;
    use exo_managed_agents::permissions::PermissionPolicy;
    use exoharness::{BasicExoHarness, Binding, WriteArtifactRequest};

    #[tokio::test]
    async fn resumed_threads_use_updated_agent_permissions() -> Result<()> {
        let temp = tempfile::TempDir::new()?;
        let config = crate::test_support::local_test_config(temp.path().join("state"));
        let state = Arc::new(BasicExoHarness::new(config.clone()).await?);
        state
            .put_binding(Binding::Llm {
                name: "fixture".into(),
                model: "fixture".into(),
                base_url: None,
                secret: None,
            })
            .await?;
        let runtime = crate::Runtime::new(
            LocalProvider::managed(
                state,
                config,
                Default::default(),
                Arc::new(cost::PricingTable::empty()),
            )?,
            None,
        );
        let source = "---\nname: permissions\nharness: basic\nconfig:\n  model: fixture\npermission_policy: {type: always_allow}\n---\nUse tools.\n";
        let definition = AgentDefinition::parse(source.into())?;
        let agent = runtime
            .create_managed_agent(&definition, "permissions")
            .await?;
        let opened = runtime
            .open_managed_thread(&agent, None, Default::default())
            .await?;
        let thread = opened.thread;
        assert_eq!(
            runtime
                .get_conversation_config(thread.as_ref())
                .await?
                .permissions
                .for_tool("shell"),
            PermissionPolicy::AlwaysAllow {}
        );
        for (name, expected) in [
            ("always_ask", PermissionPolicy::AlwaysAsk {}),
            ("always_allow", PermissionPolicy::AlwaysAllow {}),
        ] {
            agent
                .write_artifact(WriteArtifactRequest {
                    path: managed::AGENT_DEFINITION_PATH.into(),
                    contents: source.replace("always_allow", name).into_bytes(),
                })
                .await?;
            let resumed = runtime
                .open_managed_thread(
                    &agent,
                    Some(&thread.record().id.to_string()),
                    Default::default(),
                )
                .await?;
            assert!(!resumed.created);
            assert_eq!(
                runtime
                    .get_conversation_config(thread.as_ref())
                    .await?
                    .permissions
                    .for_tool("shell"),
                expected
            );
            assert_eq!(
                crate::load_conversation_config(thread.as_ref())
                    .await?
                    .permissions
                    .for_tool("shell"),
                expected
            );
        }
        runtime.shutdown().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
