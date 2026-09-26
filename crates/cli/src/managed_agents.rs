use std::path::{Path, PathBuf};

use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use clap::Args;
use executor::{
    AgentConfig, AgentHandle, ConversationConfig, ConversationHandle, FileSystemMount, Runtime,
};
use exo_managed_agents::{self as managed, AgentDefinition};
use exoharness::NewThreadRequest;
use sha2::{Digest, Sha256};

use crate::render::Verbosity;
use crate::{Commands, HarnessSelection, SandboxProviderArg};

#[derive(Debug, Args)]
pub struct ThreadArgs {
    /// Sync a saved agent from a Markdown file and start or resume a saved thread.
    #[arg(long, required_unless_present = "agent", conflicts_with = "agent")]
    pub agent_file: Option<PathBuf>,
    #[arg(long, required_unless_present = "agent_file")]
    pub agent: Option<String>,
    /// Resume a saved thread by slug or id.
    #[arg(long)]
    pub thread: Option<String>,
    /// Override the model binding for this thread.
    #[arg(long)]
    pub model: Option<String>,
    #[arg(long)]
    pub vault: Vec<String>,
    /// Use a saved environment from the selected provider.
    #[arg(long, conflicts_with_all = ["environment_file", "provider", "sandbox_image"])]
    pub environment: Option<String>,
    /// Send an environment definition to the selected provider.
    #[arg(long, conflicts_with_all = ["environment", "provider", "sandbox_image"])]
    pub environment_file: Option<PathBuf>,
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
        if self.environment.is_some() || self.environment_file.is_some() {
            return Ok(ConversationConfig::default());
        }
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
        Commands::Agent {
            command: crate::AgentCommands::Run { thread, .. },
            ..
        } => thread.agent_file.as_deref(),
        Commands::Agent {
            command:
                crate::AgentCommands::Create { file, .. } | crate::AgentCommands::Update { file, .. },
            ..
        } => Some(file.as_path()),
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
        let path = args
            .agent_file
            .as_ref()
            .context("provide --agent-file")?
            .canonicalize()
            .context("resolving agent file")?;
        let name = path.file_stem().context("agent file has no filename")?;
        let hash = format!("{:x}", Sha256::digest(path.as_os_str().as_encoded_bytes()));
        let slug = format!(
            "{}-{}",
            crate::slugify(&name.to_string_lossy()),
            &hash[..16]
        );
        eprintln!("Syncing agent resources...");
        match runtime.get_agent(&slug).await? {
            Some(agent) => {
                runtime.update_managed_agent(&agent, definition).await?;
                agent
            }
            None => runtime.create_managed_agent(definition, &slug).await?,
        }
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
    let mut environment = match (&args.environment_file, &args.environment) {
        (Some(path), _) => Some(crate::environments::load(path)?),
        (_, Some(name)) => Some(crate::environments::find(root.as_ref(), name).await?),
        _ => None,
    };
    if let Some(environment) = &mut environment {
        for mount in &args.mounts {
            let mounts = environment
                .config
                .file_system_mounts
                .get_or_insert_default();
            let mut mount = mount.clone();
            mount.host_path = crate::canonicalize_directory(Path::new(&mount.host_path))?
                .to_string_lossy()
                .into_owned();
            mounts.retain(|other| other.mount_path != mount.mount_path);
            mounts.push(mount);
        }
        environment.validate()?;
    }
    let slug = crate::generate_fun_slug();
    eprintln!("Preparing thread resources and connections...");
    let opened = runtime
        .open_managed_thread(
            &agent,
            args.thread.as_deref(),
            NewThreadRequest {
                environment,
                vaults: vaults.iter().map(|vault| vault.record().id).collect(),
                slug: Some(slug.clone()),
                name: Some(slug),
            },
        )
        .await?;
    println!("agent: {} ({})", agent.record().slug, agent.record().id);
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
