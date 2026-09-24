use std::path::{Path, PathBuf};

use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use clap::Args;
use executor::{
    AgentConfig, AgentHandle, ConversationConfig, ConversationHandle, FileSystemMount, Runtime,
};
use exo_managed_agents::{self as managed, AgentDefinition};
use exoharness::NewThreadRequest;

use crate::render::Verbosity;
use crate::{Commands, HarnessSelection, SandboxProviderArg};

#[derive(Debug, Args)]
pub struct ThreadArgs {
    /// Run a Markdown agent with temporary in-memory state.
    #[arg(long, required_unless_present = "agent", conflicts_with = "agent")]
    pub agent_file: Option<PathBuf>,
    #[arg(long, required_unless_present = "agent_file")]
    pub agent: Option<String>,
    /// Resume a saved thread by slug or id.
    #[arg(long, requires = "agent")]
    pub thread: Option<String>,
    /// Override the model binding for this thread.
    #[arg(long)]
    pub model: Option<String>,
    #[arg(long)]
    pub vault: Vec<String>,
    #[arg(long = "sandbox", value_enum)]
    provider: Option<SandboxProviderArg>,
    #[arg(long)]
    sandbox_image: Option<String>,
    /// Mount a host directory: HOST_PATH:GUEST_PATH[:ro|rw].
    #[arg(long = "mount", value_parser = crate::parse_sandbox_mount)]
    mounts: Vec<FileSystemMount>,
    #[arg(long, value_enum, default_value_t = Verbosity::default())]
    pub verbosity: Verbosity,
}

impl ThreadArgs {
    pub(crate) fn local_config(&self) -> Result<ConversationConfig> {
        let mut mounts = self.mounts.clone();
        for mount in &mut mounts {
            mount.host_path = crate::canonicalize_directory(Path::new(&mount.host_path))?
                .to_string_lossy()
                .into_owned();
        }
        for (index, mount) in mounts.iter().enumerate() {
            if mounts[..index]
                .iter()
                .any(|other| other.mount_path == mount.mount_path)
            {
                bail!("duplicate mount path: {}", mount.mount_path);
            }
        }
        if self
            .sandbox_image
            .as_ref()
            .is_some_and(|image| image.trim().is_empty())
        {
            bail!("sandbox image must not be empty");
        }
        Ok(ConversationConfig {
            sandbox_provider: self.provider.map(Into::into),
            sandbox_image: self.sandbox_image.clone(),
            mounts,
            ..Default::default()
        })
    }

    pub(crate) fn validate_remote(&self) -> Result<()> {
        if self.provider.is_some() || self.sandbox_image.is_some() || !self.mounts.is_empty() {
            bail!(
                "remote providers manage their own sandboxes; client-side sandbox overrides and mounts are not supported"
            );
        }
        Ok(())
    }
}

pub fn harness_selection(definition: &AgentDefinition) -> Result<HarnessSelection> {
    let mut selection = definition
        .frontmatter
        .harness
        .parse::<HarnessSelection>()
        .map_err(|error| anyhow!(error))?;
    if let HarnessSelection::TypeScriptModule(module) = &mut selection
        && let Some(path) = definition.path()
    {
        *module = path.parent().unwrap_or(Path::new(".")).join(&*module);
    }
    Ok(selection)
}

pub fn load_definition(command: &Commands) -> Result<Option<AgentDefinition>> {
    let path = match command {
        Commands::Chat { thread, .. } | Commands::Run { thread, .. } => {
            thread.agent_file.as_deref()
        }
        Commands::Agent {
            command: crate::AgentCommands::Create { file, .. },
            ..
        } => file.as_deref(),
        _ => None,
    };
    path.map(AgentDefinition::load).transpose()
}

pub fn local_agent_config(
    definition: &AgentDefinition,
    selection: &HarnessSelection,
    model: Option<&str>,
) -> Result<AgentConfig> {
    executor::managed_agents::agent_config(
        definition,
        crate::default_local_sandbox_provider(),
        Some(&crate::format_harness_selection(selection)),
        model,
    )
}

pub async fn open_thread(
    runtime: &Runtime,
    definition: Option<&AgentDefinition>,
    args: &ThreadArgs,
) -> Result<(Arc<dyn AgentHandle>, Arc<dyn ConversationHandle>)> {
    let agent = if let Some(definition) = definition {
        let slug = format!(
            "{}-{}",
            crate::slugify(&definition.frontmatter.name),
            exoharness::Uuid7::now()
        );
        runtime.create_managed_agent(definition, &slug).await?
    } else {
        crate::must_get_agent(
            runtime,
            args.agent
                .as_deref()
                .context("provide --agent-file or --agent")?,
        )
        .await?
    };
    let root = runtime.exoharness_handle();
    let vaults = futures::future::try_join_all(
        args.vault
            .iter()
            .map(|reference| managed::vaults::find_vault(root.as_ref(), reference)),
    )
    .await?;
    let slug = crate::generate_fun_slug();
    let opened = runtime
        .open_managed_thread(
            &agent,
            args.thread.as_deref(),
            NewThreadRequest {
                vaults: vaults.iter().map(|vault| vault.record().id).collect(),
                slug: Some(slug.clone()),
                name: Some(slug),
            },
        )
        .await?;
    println!("agent: {} ({})", agent.record().slug, agent.record().id);
    if args.agent_file.is_some() {
        println!("state: temporary (agent and thread history are discarded on exit)");
    }
    println!(
        "thread: {} ({})",
        opened.thread.record().slug,
        opened.thread.record().id
    );
    for vault in opened.thread.list_vaults().await? {
        println!("vault: {} ({})", vault.record().name, vault.record().id);
    }
    if let Some(model) = opened.info.model {
        println!("model: {model}");
    }
    if let Some(count) = opened.info.mcp_tools
        && count > 0
    {
        println!("mcp: {count} tools");
    }
    Ok((agent, opened.thread))
}

pub async fn list_threads(harness: &Runtime, agent: &str) -> Result<()> {
    let agent = managed::find_agent(harness.exoharness_handle().as_ref(), agent).await?;
    crate::print_table(
        &["THREAD", "ID", "NAME"],
        managed::list_threads(agent.as_ref())
            .await?
            .into_iter()
            .map(|thread| {
                let record = thread.record();
                vec![
                    record.slug.clone(),
                    record.id.to_string(),
                    record.name.clone(),
                ]
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests;
