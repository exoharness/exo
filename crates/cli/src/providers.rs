mod oauth;

#[cfg(test)]
mod tests;

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail, ensure};
use clap::{Args, Subcommand};
use exo_managed_agents::http::RuntimeClient;
use exoharness::{AgentId, ThreadId, Uuid7};
use serde::{Deserialize, Serialize};

pub(crate) async fn runtime(
    cli: &crate::Cli,
    client: Option<RuntimeClient>,
    definition: Option<&exo_managed_agents::AgentDefinition>,
    env: &crate::env::CliEnvironment,
) -> Result<std::sync::Arc<executor::Runtime>> {
    use crate::{AgentCommands, Commands, HarnessSelection, managed_agents};
    use executor::managed_agents::LocalAgentSetup;
    use exoharness::{BasicExoHarness, ExoHarness};
    use std::sync::Arc;

    let thread = match &cli.command {
        Commands::Agent {
            command: AgentCommands::Run { thread, .. },
            ..
        } => Some(thread),
        _ => None,
    };
    let model = thread.and_then(|args| args.model.clone());
    if let Some(client) = client {
        validate_http_command(&cli.command)?;
        if matches!(
            cli.runtime().harness,
            Some(HarnessSelection::TypeScriptModule(_))
        ) {
            bail!("a remote provider cannot load a client-side TypeScript harness module");
        }
        return Ok(Arc::new(executor::Runtime::new(
            executor::HttpProvider::with_options(
                client,
                model,
                cli.runtime()
                    .harness
                    .as_ref()
                    .map(crate::format_harness_selection),
            ),
            None,
        )));
    }

    let config = crate::build_exo_config(cli)?;
    let env_vars = env.clone().into_vars();
    let state: Arc<dyn ExoHarness> = Arc::new(BasicExoHarness::new(config.clone()).await?);
    if let Some(reference) = thread.and_then(|args| args.agent.as_deref())
        && let Some(selection) = cli.runtime().harness.as_ref()
    {
        let agent = exo_managed_agents::find_agent(state.as_ref(), reference).await?;
        crate::ensure_agent_matches_harness_selection(agent.as_ref(), selection).await?;
    }
    let setup = LocalAgentSetup {
        agent: definition
            .map(|definition| {
                managed_agents::local_agent_config(
                    definition,
                    cli.runtime()
                        .harness
                        .as_ref()
                        .context("agent definition has no harness")?,
                    None,
                )
            })
            .transpose()?,
        model,
        thread: thread
            .map(|args| args.local_config())
            .transpose()?
            .unwrap_or_default(),
    };
    let pricing = Arc::new(
        if matches!(
            cli.command,
            Commands::Agent {
                command: AgentCommands::Run { .. } | AgentCommands::Serve(_),
                ..
            } | Commands::Conversation {
                command: crate::ConversationCommands::Send { .. },
                ..
            }
        ) {
            cost::load(
                cli.runtime().pricing_path.clone(),
                cli.runtime().pricing_url.clone(),
            )
            .await
        } else {
            cost::PricingTable::empty()
        },
    );
    let provider = executor::LocalProvider::managed(state, config, env_vars, pricing)?
        .with_managed_agents(setup);
    Ok(Arc::new(executor::Runtime::new(
        provider,
        env.braintrust_runtime_config(
            cli.runtime().braintrust_api_key.clone(),
            cli.runtime().braintrust_app_url.clone(),
            cli.runtime().braintrust_api_url.clone(),
        ),
    )))
}

pub(crate) fn validate_http_command(command: &crate::Commands) -> Result<()> {
    use crate::{AgentCommands, Commands, ConversationCommands};
    match command {
        Commands::Agent {
            command: AgentCommands::Run { tui: true, .. },
            ..
        } => bail!("HTTP providers support inline chat; omit --tui"),
        Commands::Agent {
            command: AgentCommands::Run { thread, .. },
            ..
        } => thread.validate_remote(),
        Commands::Agent {
            command:
                AgentCommands::List
                | AgentCommands::Create { .. }
                | AgentCommands::Update { .. }
                | AgentCommands::Get { .. }
                | AgentCommands::Delete { .. },
            ..
        }
        | Commands::Conversation {
            command:
                ConversationCommands::List { .. }
                | ConversationCommands::Get { .. }
                | ConversationCommands::Events { .. }
                | ConversationCommands::Send { .. }
                | ConversationCommands::Delete { .. },
            ..
        }
        | Commands::Environment { .. }
        | Commands::Vault { .. } => Ok(()),
        _ => bail!("this command is not supported by the managed-agent HTTP provider"),
    }
}

#[derive(Debug, Subcommand)]
pub enum ProviderCommands {
    List,
    /// Show a profile, or the current selection when NAME is omitted.
    Get {
        name: Option<String>,
    },
    /// Clear the global provider selection without deleting profiles or credentials.
    Clear {
        /// Clear only the selection saved in this directory.
        #[arg(long)]
        local: bool,
    },
    Create(ConfigureArgs),
    Update(ConfigureArgs),
    Delete {
        name: String,
    },
    /// Persist the provider selection for future commands.
    Switch {
        name: String,
        /// Apply to this directory and its descendants instead of globally.
        #[arg(long)]
        local: bool,
        /// Selection context as key=value,key=value. Empty sends no context; omitting it resets this selection to the profile context.
        #[arg(long, value_name = "KEY=VALUE,...", value_parser = parse_context)]
        context: Option<BTreeMap<String, String>>,
    },
    Login {
        name: String,
        #[arg(long, value_parser = crate::parse_env_var_name)]
        api_key_env: Option<String>,
        #[arg(long)]
        no_browser: bool,
    },
    Logout {
        name: String,
    },
}

#[derive(Debug, Args)]
pub struct ConfigureArgs {
    name: String,
    #[arg(long, conflicts_with = "local_root")]
    url: Option<String>,
    #[arg(long, conflicts_with_all = ["api_key_env", "client_id", "scope"])]
    local_root: Option<PathBuf>,
    #[arg(long, value_parser = crate::parse_env_var_name)]
    api_key_env: Option<String>,
    #[arg(long)]
    client_id: Option<String>,
    /// OAuth scopes to request when logging in (repeatable).
    #[arg(long)]
    scope: Vec<String>,
    /// Provider context as key=value,key=value. Replaces the context; use an empty string to clear it.
    #[arg(long, value_name = "KEY=VALUE,...", value_parser = parse_context)]
    context: Option<BTreeMap<String, String>>,
}

fn parse_context(value: &str) -> std::result::Result<BTreeMap<String, String>, String> {
    let mut context = BTreeMap::new();
    if value.is_empty() {
        return Ok(context);
    }
    for entry in value.split(',') {
        let (key, value) = entry
            .split_once('=')
            .ok_or("context must be key=value,key=value")?;
        let (key, value) = (key.trim(), value.trim());
        if key.is_empty() || value.is_empty() {
            return Err("context keys and values must not be empty".into());
        }
        if context.insert(key.to_owned(), value.to_owned()).is_some() {
            return Err(format!("duplicate context key: {key}"));
        }
    }
    Ok(context)
}

#[derive(Debug)]
pub struct Selection {
    pub name: String,
    pub context: BTreeMap<String, String>,
    source: SelectionSource,
}

#[derive(Debug)]
enum SelectionSource {
    Profile,
    Global,
    Directory(PathBuf),
    Alias,
}

#[derive(Debug)]
pub struct ContextError {
    message: String,
    help: String,
}

impl std::fmt::Display for ContextError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "{}", self.message)?;
        for c in self.help.chars() {
            if c.is_control() {
                write!(f, "{}", c.escape_default())?;
            } else {
                write!(f, "{c}")?;
            }
        }
        Ok(())
    }
}

impl std::error::Error for ContextError {}

pub fn request_error(error: anyhow::Error, selection: &Selection) -> anyhow::Error {
    let Some(response) = error.downcast_ref::<exoharness::HttpResponseError>() else {
        return error;
    };
    if response.status != reqwest::StatusCode::BAD_REQUEST {
        return error;
    }
    let Ok(exo_managed_agents::http::protocol::ProviderError::ContextRequired { message, context }) =
        serde_json::from_str(&response.body)
    else {
        return error;
    };
    let mut effective = context;
    effective.extend(selection.context.clone());
    let argument = effective
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join(",");
    let (Ok(name), Ok(context)) = (
        shlex::try_quote(&selection.name),
        shlex::try_quote(&argument),
    ) else {
        return error;
    };
    let help = match &selection.source {
        SelectionSource::Profile => format!("Run: exo provider update {name} --context {context}"),
        SelectionSource::Global => format!("Run: exo provider switch {name} --context {context}"),
        SelectionSource::Directory(path) => {
            let directory = path.to_string_lossy();
            let Ok(directory) = shlex::try_quote(&directory) else {
                return error;
            };
            format!("Run: (cd {directory} && exo provider switch {name} --local --context {context})")
        }
        SelectionSource::Alias => "Saved aliases retain their context. Use a remote ID with the required context, or recreate the alias.".into(),
    };
    error.context(ContextError {
        message: message.trim().escape_debug().to_string(),
        help,
    })
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Connection {
    Local { root: PathBuf },
    Http { endpoint: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Profile {
    pub id: Uuid7,
    pub connection: Connection,
    pub api_key_env: Option<String>,
    pub account_id: Option<String>,
    pub client_id: Option<String>,
    #[serde(default)]
    pub stored_credentials: bool,
    #[serde(default)]
    pub scopes: Vec<String>,
    #[serde(default)]
    pub context: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct AgentAlias {
    pub provider: String,
    pub connection: Connection,
    pub account_id: String,
    pub agent_id: AgentId,
    #[serde(default)]
    pub context: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct ThreadAlias {
    #[serde(flatten)]
    pub agent: AgentAlias,
    pub thread_id: ThreadId,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Config {
    #[serde(default)]
    profiles: BTreeMap<String, Profile>,
    default: Option<String>,
    default_context: Option<BTreeMap<String, String>>,
    #[serde(default)]
    directory_defaults: BTreeMap<PathBuf, String>,
    #[serde(default)]
    directory_contexts: BTreeMap<PathBuf, BTreeMap<String, String>>,
    #[serde(default)]
    agents: BTreeMap<String, AgentAlias>,
    #[serde(default)]
    threads: BTreeMap<String, ThreadAlias>,
}

pub struct Store {
    config: Config,
    directory: PathBuf,
}

impl Store {
    pub fn load(directory: PathBuf) -> Result<Self> {
        let saved = read_config(&directory)?;
        let config = saved
            .as_deref()
            .map(serde_json::from_slice)
            .transpose()
            .context("invalid Exo provider configuration")?
            .unwrap_or_default();
        Ok(Self { config, directory })
    }

    fn update<T>(&mut self, operation: impl FnOnce(&mut Config) -> Result<T>) -> Result<T> {
        use std::io::Write;
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.directory)?;
        let lock = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(self.directory.join("providers.lock"))?;
        lock.lock()?;
        let mut config = Self::load(self.directory.clone())?.config;
        let result = operation(&mut config)?;
        let mut file = tempfile::NamedTempFile::new_in(&self.directory)?;
        serde_json::to_writer_pretty(&mut file, &config)?;
        file.flush()?;
        file.as_file().sync_all()?;
        file.persist(self.directory.join("providers.json"))?;
        self.config = config;
        Ok(result)
    }

    fn replace_profile(&mut self, name: &str, prior: &Profile, profile: Profile) -> Result<()> {
        self.update(|config| {
            if config.profiles.get(name) != Some(prior) {
                bail!("provider {name} changed in another process; retry the command");
            }
            config.profiles.insert(name.to_owned(), profile);
            Ok(())
        })
    }

    pub fn ensure_agent_alias_available(&self, name: &str) -> Result<()> {
        if let Some(alias) = self.config.agents.get(name) {
            bail!(
                "agent alias {name} already belongs to provider {} (account {})",
                alias.provider,
                alias.account_id
            );
        }
        Ok(())
    }

    pub fn unpin_agent(&mut self, provider: &str, account: &str, agent_id: AgentId) -> Result<()> {
        self.update(|config| {
            let matches = |alias: &AgentAlias| {
                alias.provider == provider
                    && alias.account_id == account
                    && alias.agent_id == agent_id
            };
            config.agents.retain(|_, alias| !matches(alias));
            config.threads.retain(|_, alias| !matches(&alias.agent));
            Ok(())
        })
    }

    pub fn unpin_thread(
        &mut self,
        provider: &str,
        account: &str,
        thread_id: ThreadId,
    ) -> Result<()> {
        self.update(|config| {
            config.threads.retain(|_, alias| {
                !(alias.agent.provider == provider
                    && alias.agent.account_id == account
                    && alias.thread_id == thread_id)
            });
            Ok(())
        })
    }

    pub fn profile(&self, name: &str) -> Result<&Profile> {
        self.config
            .profiles
            .get(name)
            .with_context(|| format!("provider {name} is not configured"))
    }

    pub fn selected(
        &self,
        explicit: Option<&str>,
        agent: Option<&str>,
        thread: Option<&str>,
    ) -> Result<Option<Selection>> {
        if let (Some(agent), Some(thread)) = (
            agent.and_then(|name| self.config.agents.get(name)),
            thread.and_then(|name| self.config.threads.get(name)),
        ) && (agent.provider != thread.agent.provider
            || agent.account_id != thread.agent.account_id
            || agent.agent_id != thread.agent.agent_id
            || agent.context != thread.agent.context)
        {
            bail!("agent and thread aliases belong to different agents, accounts, or contexts");
        }
        let alias = thread
            .and_then(|name| self.config.threads.get(name))
            .map(|alias| &alias.agent)
            .or_else(|| agent.and_then(|name| self.config.agents.get(name)));
        if let Some(alias) = alias {
            if explicit.is_some_and(|name| name != alias.provider) {
                bail!(
                    "alias belongs to provider {}; use its remote ID to address another provider",
                    alias.provider
                );
            }
            let profile = self.profile(&alias.provider)?;
            if profile.connection != alias.connection {
                bail!(
                    "provider configuration changed for this alias; restore its endpoint or use a remote ID"
                );
            }
            return Ok(Some(Selection {
                name: alias.provider.clone(),
                context: alias.context.clone(),
                source: SelectionSource::Alias,
            }));
        }
        let (name, context, source) = if let Some(name) = explicit {
            (name, None, SelectionSource::Profile)
        } else if let Some((path, name)) = std::env::current_dir()?
            .ancestors()
            .find_map(|path| self.config.directory_defaults.get_key_value(path))
        {
            (
                name.as_str(),
                self.config.directory_contexts.get(path),
                SelectionSource::Directory(path.clone()),
            )
        } else if let Some(name) = self.config.default.as_deref() {
            (
                name,
                self.config.default_context.as_ref(),
                SelectionSource::Global,
            )
        } else {
            return Ok(None);
        };
        let mut selection = self.selection(name, context)?;
        selection.source = source;
        Ok(Some(selection))
    }

    fn selection(
        &self,
        name: &str,
        context: Option<&BTreeMap<String, String>>,
    ) -> Result<Selection> {
        Ok(Selection {
            name: name.to_owned(),
            context: context.unwrap_or(&self.profile(name)?.context).clone(),
            source: SelectionSource::Profile,
        })
    }

    pub fn resolve_aliases(&self, command: &mut crate::Commands, account: &str) -> Result<()> {
        let (agent, thread) = crate::command_refs_mut(command);
        if let Some(reference) = agent
            && let Some(alias) = self.config.agents.get(reference)
        {
            ensure!(
                alias.account_id == account,
                "agent alias {reference} belongs to a different account; log in to that account or use the remote ID"
            );
            *reference = alias.agent_id.to_string();
        }
        if let Some(reference) = thread
            && let Some(alias) = self.config.threads.get(reference)
        {
            ensure!(
                alias.agent.account_id == account,
                "thread alias {reference} belongs to a different account; log in to that account or use the remote ID"
            );
            *reference = alias.thread_id.to_string();
        }
        Ok(())
    }

    pub fn pin_agent(
        &mut self,
        name: String,
        selection: &Selection,
        account: &str,
        agent_id: AgentId,
    ) -> Result<()> {
        let alias = AgentAlias {
            provider: selection.name.clone(),
            connection: self.profile(&selection.name)?.connection.clone(),
            account_id: account.into(),
            agent_id,
            context: selection.context.clone(),
        };
        self.update(|config| {
            if let Some(existing) = config.agents.get(&name)
                && existing != &alias
            {
                bail!(
                    "agent alias {name} already belongs to provider {} (account {})",
                    existing.provider,
                    existing.account_id
                );
            }
            config.agents.insert(name, alias);
            Ok(())
        })
    }

    pub fn pin_thread(
        &mut self,
        name: String,
        selection: &Selection,
        account: &str,
        agent_id: AgentId,
        thread_id: ThreadId,
    ) -> Result<()> {
        let agent = AgentAlias {
            provider: selection.name.clone(),
            connection: self.profile(&selection.name)?.connection.clone(),
            account_id: account.into(),
            agent_id,
            context: selection.context.clone(),
        };
        let alias = ThreadAlias { agent, thread_id };
        self.update(|config| {
            if let Some(existing) = config.threads.get(&name)
                && existing != &alias
            {
                bail!(
                    "thread alias {name} already belongs to provider {} (account {})",
                    existing.agent.provider,
                    existing.agent.account_id
                );
            }
            config.threads.insert(name, alias);
            Ok(())
        })
    }

    pub async fn client(&self, selection: &Selection) -> Result<(RuntimeClient, String)> {
        let name = &selection.name;
        let mut profile = self.profile(name)?.clone();
        profile.context = selection.context.clone();
        let mut client = profile.client()?;
        if let Some(variable) = &profile.api_key_env {
            let token = std::env::var(variable).with_context(|| {
                format!("provider credential environment variable {variable} is not set")
            })?;
            if token.trim().is_empty() {
                bail!("provider credential is empty");
            }
            client = client.with_bearer_token(token);
        } else if profile.stored_credentials {
            return oauth::client(&profile, &self.directory)
                .await
                .with_context(|| format!("authenticating provider {name}"));
        }
        let account = client.identity().await?.account_id;
        if profile
            .account_id
            .as_ref()
            .is_some_and(|saved| saved != &account)
        {
            bail!(
                "provider {name} is authenticated as a different account; run exo provider login {name} to switch explicitly"
            );
        }
        Ok((client, account))
    }
}

fn read_config(directory: &Path) -> Result<Option<Vec<u8>>> {
    match std::fs::read(directory.join("providers.json")) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

impl Profile {
    fn client(&self) -> Result<RuntimeClient> {
        let Connection::Http { endpoint } = &self.connection else {
            bail!("local providers do not have an HTTP client");
        };
        RuntimeClient::new(endpoint)?.with_context(&self.context)
    }
}

fn absolute_root(path: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(path)?;
    Ok(path.canonicalize()?)
}

fn print_selection(store: &Store) -> Result<()> {
    let Some(selection) = store.selected(None, None, None)? else {
        println!("provider: local (built-in; no saved selection)");
        return Ok(());
    };
    println!("provider: {}", selection.name.escape_debug());
    match selection.source {
        SelectionSource::Global => println!("selection: global"),
        SelectionSource::Directory(path) => println!(
            "selection: directory {}",
            path.display().to_string().escape_debug()
        ),
        SelectionSource::Profile => println!("selection: explicit profile"),
        SelectionSource::Alias => println!("selection: saved alias"),
    }
    if !selection.context.is_empty() {
        println!("context: {}", serde_json::to_string(&selection.context)?);
    }
    Ok(())
}

pub async fn run(command: Option<&ProviderCommands>, store: &mut Store) -> Result<()> {
    let current = ProviderCommands::Get { name: None };
    let command = command.unwrap_or(&current);
    match command {
        ProviderCommands::Get { name: None } => print_selection(store)?,
        ProviderCommands::Clear { local } => {
            let directory = local.then(std::env::current_dir).transpose()?;
            store.update(|config| {
                if let Some(directory) = &directory {
                    config.directory_defaults.remove(directory);
                    config.directory_contexts.remove(directory);
                } else {
                    config.default = None;
                    config.default_context = None;
                }
                Ok(())
            })?;
            print_selection(store)?;
        }
        ProviderCommands::List => {
            print_selection(store)?;
            println!();
            crate::print_table(
                &["PROVIDER", "CONNECTION", "ACCOUNT"],
                store
                    .config
                    .profiles
                    .iter()
                    .map(|(name, profile)| {
                        vec![
                            name.clone(),
                            match &profile.connection {
                                Connection::Local { root } => root.display().to_string(),
                                Connection::Http { endpoint, .. } => endpoint.clone(),
                            },
                            profile.account_id.clone().unwrap_or_default(),
                        ]
                    })
                    .collect(),
            )?;
        }
        ProviderCommands::Get { name: Some(name) } => {
            #[derive(Serialize)]
            struct View<'a> {
                #[serde(flatten)]
                profile: &'a Profile,
                effective_context: BTreeMap<String, String>,
            }
            let selected = store.selected(None, None, None)?;
            let profile = store.profile(name)?;
            let effective_context = selected
                .filter(|selection| selection.name == *name)
                .map(|selection| selection.context)
                .unwrap_or_else(|| profile.context.clone());
            println!(
                "{}",
                serde_json::to_string_pretty(&View {
                    profile,
                    effective_context
                })?
            );
        }
        ProviderCommands::Create(args) | ProviderCommands::Update(args) => {
            let exists = store.config.profiles.contains_key(&args.name);
            if exists != matches!(command, ProviderCommands::Update(_)) {
                bail!(
                    "provider {} {}; use create or update accordingly",
                    args.name,
                    if exists {
                        "already exists"
                    } else {
                        "does not exist"
                    }
                );
            }
            if args.name.trim().is_empty() {
                bail!("provider name must not be empty");
            }
            let prior = store.config.profiles.get(&args.name).cloned();
            let connection = match (&args.url, &args.local_root) {
                (Some(url), _) => {
                    let client = RuntimeClient::new(url)?;
                    let mut endpoint = client.endpoint().clone();
                    endpoint.set_path(client.endpoint().path().trim_end_matches('/'));
                    Connection::Http {
                        endpoint: endpoint.to_string(),
                    }
                }
                (_, Some(root)) => Connection::Local {
                    root: absolute_root(root)?,
                },
                _ => prior
                    .as_ref()
                    .context("provide --url or --local-root")?
                    .connection
                    .clone(),
            };
            let mut profile = prior.clone().unwrap_or(Profile {
                id: Uuid7::now(),
                connection: connection.clone(),
                api_key_env: None,
                account_id: None,
                client_id: None,
                stored_credentials: false,
                scopes: vec![],
                context: BTreeMap::new(),
            });
            if let Some(context) = &args.context {
                profile.context = context.clone();
            }
            if matches!(connection, Connection::Local { .. }) && !profile.context.is_empty() {
                bail!("context is only supported for HTTP providers");
            }
            if profile.connection != connection {
                oauth::clear(&profile, &store.directory).await?;
                profile.account_id = None;
                profile.stored_credentials = false;
                profile.id = Uuid7::now();
            }
            profile.connection = connection;
            if let Some(variable) = &args.api_key_env {
                profile.api_key_env = Some(variable.clone());
            }
            if let Some(client_id) = &args.client_id {
                profile.client_id = Some(client_id.clone());
            }
            if !args.scope.is_empty() {
                profile.scopes = args.scope.clone();
            }
            store.update(|config| {
                if config.profiles.get(&args.name) != prior.as_ref() {
                    bail!(
                        "provider {} changed in another process; retry the command",
                        args.name
                    );
                }
                config.profiles.insert(args.name.clone(), profile);
                Ok(())
            })?;
            eprintln!("Saved provider {}.", args.name);
            if !exists
                && matches!(
                    store.profile(&args.name)?.connection,
                    Connection::Http { .. }
                )
            {
                eprintln!(
                    "Run exo provider login {} to verify the connection and authenticate.",
                    shlex::try_quote(&args.name)?
                );
            }
        }
        ProviderCommands::Delete { name } => {
            let profile = store.profile(name)?.clone();
            oauth::clear(&profile, &store.directory).await?;
            store.update(|config| {
                if config.profiles.get(name) != Some(&profile) {
                    bail!("provider {name} changed in another process; retry the command");
                }
                config.profiles.remove(name);
                config.directory_defaults.retain(|_, value| value != name);
                config
                    .directory_contexts
                    .retain(|path, _| config.directory_defaults.contains_key(path));
                if config.default.as_ref() == Some(name) {
                    config.default = None;
                    config.default_context = None;
                }
                Ok(())
            })?;
        }
        ProviderCommands::Switch {
            name,
            local,
            context,
        } => {
            let directory = local.then(std::env::current_dir).transpose()?;
            store.update(|config| {
                let profile = config
                    .profiles
                    .get(name)
                    .with_context(|| format!("provider {name} is not configured"))?;
                if matches!(profile.connection, Connection::Local { .. })
                    && context.as_ref().is_some_and(|context| !context.is_empty())
                {
                    bail!("context is only supported for HTTP providers");
                }
                if let Some(directory) = directory {
                    config
                        .directory_defaults
                        .insert(directory.clone(), name.clone());
                    if let Some(context) = context {
                        config.directory_contexts.insert(directory, context.clone());
                    } else {
                        config.directory_contexts.remove(&directory);
                    }
                } else {
                    config.default = Some(name.clone());
                    config.default_context = context.clone();
                }
                Ok(())
            })?;
            let selected = store.selection(name, context.as_ref())?;
            println!(
                "provider: {name} ({})",
                if *local {
                    "this directory and descendants"
                } else {
                    "global"
                }
            );
            if !selected.context.is_empty() {
                println!("context: {}", serde_json::to_string(&selected.context)?);
            }
        }
        ProviderCommands::Login {
            name,
            api_key_env,
            no_browser,
        } => {
            let prior = store.profile(name)?.clone();
            let mut profile = prior.clone();
            let account =
                if let Some(variable) = api_key_env.as_ref().or(profile.api_key_env.as_ref()) {
                    let token = std::env::var(variable)
                        .with_context(|| format!("{variable} is not set"))?;
                    if token.trim().is_empty() {
                        bail!("API key is empty");
                    }
                    let account = profile
                        .client()?
                        .with_bearer_token(token)
                        .identity()
                        .await?
                        .account_id;
                    profile.api_key_env = Some(variable.clone());
                    account
                } else {
                    oauth::login(&profile, *no_browser, &store.directory).await?
                };
            if profile.api_key_env.is_none() {
                profile.stored_credentials = true;
            }
            profile.account_id = Some(account.clone());
            if let Err(error) = store.replace_profile(name, &prior, profile.clone()) {
                if profile.api_key_env.is_none()
                    && !Store::load(store.directory.clone())?
                        .config
                        .profiles
                        .values()
                        .any(|current| current.id == profile.id)
                {
                    oauth::clear(&profile, &store.directory)
                        .await
                        .with_context(|| {
                            format!("{error}; removing superseded login credentials")
                        })?;
                }
                return Err(error);
            }
            println!("provider: {name}; account: {account}");
        }
        ProviderCommands::Logout { name } => {
            let prior = store.profile(name)?.clone();
            oauth::clear(&prior, &store.directory).await?;
            let mut profile = prior.clone();
            profile.id = Uuid7::now();
            profile.api_key_env = None;
            profile.stored_credentials = false;
            profile.account_id = None;
            store.replace_profile(name, &prior, profile)?;
        }
    }
    Ok(())
}
