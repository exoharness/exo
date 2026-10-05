pub(crate) mod config;
pub mod service;
#[cfg(feature = "native")]
pub use crate::native::config::agent_config;
pub use config::{
    HarnessModules, TypeScriptHarnessPreset, agent_config_with_modules,
    model_credential_destination,
};

use std::{path::Path, sync::Arc};

use anyhow::{Context, Result};
use async_trait::async_trait;
use exo_managed_agents::{self as managed, AgentBackend, AgentDefinition};
use exoharness::{AgentHandle, ExoHarness, ThreadHandle};

use crate::{AgentConfig, ConversationConfig, ConversationModelConfig, LocalProvider};

#[derive(Clone, Default)]
pub struct LocalAgentSetup {
    pub agent: Option<AgentConfig>,
    pub model: Option<String>,
    pub thread: ConversationConfig,
    pub egress_policy: Option<exoharness::EgressPolicy>,
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
        if let Some(caller) = self.state.caller() {
            let frontmatter = &definition.frontmatter;
            if !matches!(
                frontmatter.harness.as_str(),
                "basic" | "rlm" | "codex" | "claude-code" | "cursor" | "cursor-sdk" | "pi"
            ) || !frontmatter.tools.is_empty()
                || frontmatter.tool_creation
                || !frontmatter.adapters.is_empty()
            {
                caller.policy.check_operator(&caller.principal).await?;
            }
        }
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
        thread.claim_local_session().await?;
        let agent_config = crate::load_agent_config(agent).await?;
        let current_model = crate::get_conversation_model_override(thread).await?;
        let preferred = current_model
            .as_ref()
            .map(|config| config.model.as_str())
            .unwrap_or(&agent_config.model);
        let model = self
            .managed
            .model
            .as_deref()
            .unwrap_or(preferred)
            .to_owned();
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
        if let Some(policy) = &self.managed.egress_policy {
            config.egress_policy = Some(policy.clone());
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
            let mut mounts = config.mounts;
            if let Some(previous) = &config.environment
                && let Some(previous_mounts) = &previous.config.file_system_mounts
            {
                for mount in previous_mounts {
                    mounts.retain(|other| other.mount_path != mount.mount_path);
                }
            }
            for mount in environment.config.file_system_mounts.iter().flatten() {
                mounts.retain(|other| other.mount_path != mount.mount_path);
                mounts.push(mount.clone());
            }
            config.environment = Some(environment.clone());
            if environment.config.policy.is_some() {
                config.egress_policy = None;
            }
            config.sandbox_provider = Some(environment.config.provider.clone());
            config.sandbox_image = Some(environment.config.image.clone());
            config.mounts = mounts;
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
        if !config.resources.is_empty() {
            let thread = agent
                .get_thread(&thread.record().id)
                .await?
                .context("thread disappeared before resource preparation")?;
            let provider = config.effective_sandbox_provider(&agent_config);
            let mut preparations = self
                .resource_preparations
                .lock()
                .expect("resource preparations poisoned");
            while let Some(result) = preparations.try_join_next() {
                result?;
            }
            preparations.spawn(async move {
                if let Err(error) = thread.materialize_resources(config.resources, provider).await {
                    tracing::debug!(%error, "background resource preparation failed; the next command will retry");
                }
            });
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
    pub fn with_managed_agents(mut self, setup: LocalAgentSetup) -> Self {
        self.managed = setup;
        self
    }
}
