use std::path::{Path, PathBuf};

use exo_mcp::McpToolSet;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use clap::Args;
use executor::{
    AgentConfig, AgentSandboxConfig, ConversationModelConfig, CreateConversationRequest,
    FileSystemMount, Harness, HarnessAgent, HarnessConversation,
};
use exo_managed_agents::{self as managed, AgentBackend, AgentDefinition};
use exoharness::vault::{VaultId, compose_vaults, global_vault};
use exoharness::{AgentHandle, ExoHarness};
use lingua::Message;

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
    #[arg(long = "provider", visible_alias = "sandbox", value_enum)]
    provider: Option<SandboxProviderArg>,
    #[arg(long)]
    sandbox_image: Option<String>,
    /// Mount a host directory: HOST_PATH:GUEST_PATH[:ro|rw].
    #[arg(long = "mount", value_parser = crate::parse_sandbox_mount)]
    mounts: Vec<FileSystemMount>,
    #[arg(long, value_enum, default_value_t = Verbosity::default())]
    pub verbosity: Verbosity,
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

struct CliBackend<'a> {
    harness: &'a dyn Harness,
    config: AgentConfig,
}

#[async_trait]
impl AgentBackend for CliBackend<'_> {
    fn exoharness(&self) -> Arc<dyn ExoHarness> {
        self.harness.exoharness_handle()
    }

    async fn configure_agent(
        &self,
        agent: &Arc<dyn AgentHandle>,
        _definition: &AgentDefinition,
    ) -> Result<()> {
        crate::must_get_agent(self.harness, &agent.record().id.to_string())
            .await?
            .put_config(self.config.clone())
            .await
    }
}

pub fn load_definition(command: &Commands) -> Result<Option<AgentDefinition>> {
    let path = match command {
        Commands::Chat { thread, .. } | Commands::Run { thread, .. } => {
            thread.agent_file.as_deref()
        }
        Commands::Agent {
            command: crate::AgentCommands::Create { file, .. },
        } => file.as_deref(),
        _ => None,
    };
    path.map(AgentDefinition::load).transpose()
}

#[derive(Default)]
pub struct PreparedMcp {
    pub tools: Arc<McpToolSet>,
    selection: Option<managed::vaults::VaultSelection>,
    attached_vaults: Vec<VaultId>,
    selection_is_new: bool,
}

pub async fn connect_mcp(
    root: &dyn executor::ExoHarness,
    definition: Option<&AgentDefinition>,
    command: &mut Commands,
) -> Result<PreparedMcp> {
    let args = match command {
        Commands::Chat { thread, .. } | Commands::Run { thread, .. } => thread,
        _ => return Ok(PreparedMcp::default()),
    };
    let attached = futures::future::try_join_all(
        args.vault
            .iter()
            .map(|v| managed::vaults::find_vault(root, v)),
    )
    .await?;
    let attached_vaults: Vec<_> = attached.iter().map(|v| v.record().id).collect();
    let mut vaults = vec![global_vault(root).await?];
    let mut selection = None;
    let saved;
    let definition = if let Some(definition) = definition {
        Some(definition)
    } else {
        let agent = managed::find_agent(
            root,
            args.agent
                .as_deref()
                .context("provide --agent-file or --agent")?,
        )
        .await?;
        args.agent = Some(agent.record().id.to_string());
        vaults = agent.list_vaults().await?;
        if let Some(reference) = &args.thread {
            let thread = managed::find_thread(agent.as_ref(), reference).await?;
            if !attached_vaults.is_empty() && thread.record().vaults != attached_vaults {
                bail!("cannot switch vaults on an existing thread; start a new thread");
            }
            vaults = thread.list_vaults().await?;
            selection = managed::vaults::load_selection(thread.as_ref()).await?;
        }
        saved = managed::load_definition(agent.as_ref()).await?;
        saved.as_ref()
    };
    vaults = compose_vaults(root, vaults, &attached_vaults).await?;
    let resolved = match definition {
        Some(definition) => definition.resolve_mcp_servers(&()).await?,
        None => Vec::new(),
    };
    let servers = resolved.as_slice();
    if servers.is_empty() && attached_vaults.is_empty() && vaults.len() == 1 {
        return Ok(PreparedMcp {
            attached_vaults,
            ..Default::default()
        });
    }
    let selection_is_new = selection.is_none();
    let selection = match selection {
        Some(selection) => selection,
        None => managed::vaults::VaultSelection::from_vaults(&vaults, servers).await?,
    };
    selection.validate_servers(servers)?;
    let credentials = managed::vaults::VaultMcpCredentials::new(vaults, selection.clone());
    Ok(PreparedMcp {
        tools: Arc::new(McpToolSet::connect_with_provider(servers, Arc::new(credentials)).await?),
        selection: Some(selection),
        attached_vaults,
        selection_is_new,
    })
}

pub async fn create_agent(
    harness: &dyn Harness,
    definition: &AgentDefinition,
    slug: &str,
    selection: Option<&HarnessSelection>,
    model: Option<&str>,
) -> Result<Arc<dyn HarnessAgent>> {
    let model = model.unwrap_or(&definition.frontmatter.config.model);
    if model.trim().is_empty() {
        bail!("model must not be empty");
    }
    let selection = match selection {
        Some(selection) => selection.clone(),
        None => harness_selection(definition)?,
    };
    let typescript = crate::build_typescript_harness_config(Some(&selection), None, &[])?;
    let backend = CliBackend {
        harness,
        config: AgentConfig {
            instructions: vec![Message::System {
                content: lingua::universal::UserContent::String(definition.system_prompt()),
            }],
            harness: crate::to_agent_harness_kind(selection.harness_kind()),
            typescript,
            enable_agent_tool_creation: false,
            sandbox: AgentSandboxConfig {
                image: selection.default_sandbox_image().map(str::to_string),
                provider: crate::default_local_sandbox_provider(),
                scope: Default::default(),
                mounts: Vec::new(),
                enable_networking: true,
            },
            model: model.to_string(),
            max_output_tokens: None,
            max_tool_round_trips: None,
            braintrust: None,
        },
    };
    let agent = managed::create_agent(&backend, definition, slug).await?;
    crate::must_get_agent(harness, &agent.record().id.to_string()).await
}

pub async fn open_thread(
    harness: &dyn Harness,
    definition: Option<&AgentDefinition>,
    selection: Option<&HarnessSelection>,
    args: &ThreadArgs,
    prepared_mcp: &PreparedMcp,
    vault_config: &exoharness::BasicExoHarnessConfig,
) -> Result<(Arc<dyn HarnessAgent>, Arc<dyn HarnessConversation>)> {
    let mcp = &prepared_mcp.tools;
    let mut mounts = args.mounts.clone();
    for mount in &mut mounts {
        mount.host_path = crate::canonicalize_directory(&PathBuf::from(&mount.host_path))?
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
    if args
        .sandbox_image
        .as_ref()
        .is_some_and(|image| image.trim().is_empty())
    {
        bail!("sandbox image must not be empty");
    }
    let (agent, resolved_model) = if let Some(definition) = definition {
        let model = resolve_model(
            harness,
            args.model.as_deref(),
            &definition.frontmatter.config.model,
        )
        .await?;
        let slug = crate::slugify(&definition.frontmatter.name);
        let agent = create_agent(harness, definition, &slug, selection, Some(&model)).await?;
        (agent, Some(model))
    } else {
        let agent = crate::must_get_agent(
            harness,
            args.agent
                .as_deref()
                .context("provide --agent-file or --agent")?,
        )
        .await?;
        if let Some(selection) = selection {
            crate::ensure_agent_matches_harness_selection(agent.as_ref(), selection).await?;
        }
        (agent, None)
    };
    let existing_conversation = match &args.thread {
        Some(reference) => Some(
            agent
                .get_conversation(reference)
                .await?
                .with_context(|| format!("thread not found: {reference}"))?,
        ),
        None => None,
    };
    let agent_config = agent.config().await?;
    let (model, override_model) = match resolved_model {
        Some(model) => (model, false),
        None => {
            let current_model = match &existing_conversation {
                Some(conversation) => conversation.model_override().await?,
                None => None,
            };
            let preferred = current_model
                .as_ref()
                .map(|config| config.model.as_str())
                .unwrap_or(&agent_config.model);
            let model = resolve_model(harness, args.model.as_deref(), preferred).await?;
            let override_model = args.model.is_some() || model != preferred;
            (model, override_model)
        }
    };
    let config_changed =
        args.provider.is_some() || args.sandbox_image.is_some() || !mounts.is_empty();
    let mut config = match &existing_conversation {
        Some(conversation) => conversation.config().await?,
        None => Default::default(),
    };
    if let Some(provider) = args.provider {
        config.sandbox_provider = Some(provider.into());
    }
    if let Some(image) = &args.sandbox_image {
        config.sandbox_image = Some(image.clone());
    }
    for mount in mounts {
        config
            .mounts
            .retain(|other| other.mount_path != mount.mount_path);
        config.mounts.push(mount);
    }
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
            vault_config.validate_secret_mount(Path::new(&mount.host_path))?;
        }
    }
    if let Some(selection) = &prepared_mcp.selection
        && *provider == exoharness::SandboxProvider::LocalProcess
        && (selection.vaults.len() > 1 || selection.bindings.iter().any(|b| b.secret.is_some()))
    {
        bail!(
            "vault-backed chat requires an isolated sandbox; local-process can read host credentials"
        );
    }
    let conversation = match existing_conversation {
        Some(conversation) => conversation,
        None => {
            let slug = crate::generate_fun_slug();
            agent
                .create_conversation(CreateConversationRequest {
                    vaults: prepared_mcp.attached_vaults.clone(),
                    slug: Some(slug.clone()),
                    name: Some(slug),
                    ..Default::default()
                })
                .await?
        }
    };
    if let Some(selection) = &prepared_mcp.selection {
        if prepared_mcp.selection_is_new {
            selection
                .save(conversation.exoharness_handle().as_ref())
                .await?;
        }
        for vault in &selection.vaults {
            println!("vault: {} ({})", vault.name, vault.id);
        }
    }
    if config_changed {
        conversation.put_config(config).await?;
    }
    if override_model {
        conversation
            .put_model_override(Some(ConversationModelConfig {
                model: model.clone(),
                max_output_tokens: None,
            }))
            .await?;
    }
    println!("agent: {} ({})", agent.record().slug, agent.record().id);
    if args.agent_file.is_some() {
        println!("state: temporary (agent and thread history are discarded on exit)");
    }
    println!(
        "thread: {} ({})",
        conversation.record().slug,
        conversation.record().id
    );
    println!("model: {model}");
    if !mcp.tools().is_empty() {
        println!("mcp: {} tools", mcp.tools().len());
    }
    let inventory = serde_json::to_value(mcp.tools())?;
    let previous = conversation
        .exoharness_handle()
        .get_events(Some(exoharness::EventQuery {
            direction: Some(exoharness::EventQueryDirection::Desc),
            limit: Some(1),
            types: Some(vec![exoharness::EventKind::custom("mcp_tools")]),
            ..Default::default()
        }))
        .await?;
    if (!mcp.tools().is_empty() || !previous.events.is_empty())
        && !matches!(previous.events.first().map(|event| &event.data),
            Some(executor::EventData::Custom { payload, .. }) if payload == &inventory)
    {
        conversation
            .exoharness_handle()
            .add_events(exoharness::AddEventsRequest {
                session_id: None,
                turn_id: None,
                data: vec![executor::EventData::Custom {
                    event_type: "mcp_tools".to_string(),
                    payload: inventory,
                }],
            })
            .await?;
    }
    Ok((agent, conversation))
}

async fn resolve_model(
    harness: &dyn Harness,
    requested: Option<&str>,
    preferred: &str,
) -> Result<String> {
    let registered = crate::list_model_bindings(harness.exoharness_handle().as_ref())
        .await?
        .into_iter()
        .map(|binding| binding.name)
        .collect::<Vec<_>>();
    let model = requested.unwrap_or(preferred);
    if registered.iter().any(|name| name == model) {
        return Ok(model.to_string());
    }
    if let Some(requested) = requested {
        bail!(
            "model is not registered: {requested}; register it with `exo model create {requested} --secret <secret>`"
        );
    }
    let model = registered.first().context(
        "no model is registered; set one up first:\n  \
         exo secret create openai --env OPENAI_API_KEY\n  \
         exo model create gpt-5.5 --secret openai",
    )?;
    eprintln!("model {preferred} is not registered; using {model}");
    Ok(model.clone())
}

#[cfg(test)]
mod tests;
