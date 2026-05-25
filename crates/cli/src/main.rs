mod env;
#[cfg(test)]
mod env_tests;
#[cfg(test)]
mod mount_tests;
#[cfg(test)]
mod naming_tests;
#[cfg(test)]
mod repl_tests;
#[cfg(test)]
mod secret_tests;
mod tui;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use clap::{Parser, Subcommand, ValueEnum};
use executor::{
    AgentHarnessKind, BasicExoHarness, BasicExoHarnessConfig, BasicHarness, Binding,
    BraintrustProject, BraintrustRuntimeConfig, BraintrustTracingConfig, ConversationModelConfig,
    CreateAgentRequest, CreateConversationRequest, EventQuery, EventQueryDirection, ExoHarness,
    FileSystemMount, FileSystemMountMode, ForkConversationRequest, Harness, HarnessAgent,
    HarnessConversation, PutSecretRequest, RlmHarness, SANDBOX_MAIN_MOUNT_DIR,
    SandboxBackendChoice, Secret, SecretBackendChoice, SendRequest, TypeScriptHarness,
    TypeScriptHarnessConfig, Uuid7, load_agent_config,
};
use lingua::Message;
use lingua::universal::{AssistantContent, AssistantContentPart, ToolContentPart, UserContent};

use crate::env::CliEnvironment;
use tui::run_chat_repl;

#[derive(Debug, Parser)]
#[command(name = "exo")]
#[command(about = "CLI for harness implementations")]
#[command(
    after_help = "Runtime options:\n  --braintrust-api-key <BRAINTRUST_API_KEY>\n  --braintrust-app-url <BRAINTRUST_APP_URL>\n  --braintrust-api-url <BRAINTRUST_API_URL>\n\nThese options are accepted globally, including after subcommands, but are hidden from subcommand help to reduce noise."
)]
struct Cli {
    #[arg(long, global = true, default_value = ".exo")]
    root: PathBuf,
    #[arg(long, global = true, value_enum)]
    harness: Option<HarnessKind>,
    #[arg(long, global = true, value_enum, env = "EXO_SECRET_BACKEND")]
    secret_backend: Option<SecretBackendArg>,
    #[arg(long, global = true, value_enum, env = "EXO_SANDBOX_BACKEND")]
    sandbox_backend: Option<SandboxBackendArg>,
    #[arg(long, global = true, env = "EXO_MASTER_KEY_PATH")]
    master_key_path: Option<PathBuf>,
    #[arg(long, global = true)]
    env_file: Option<PathBuf>,
    #[arg(long, global = true)]
    env_file_if_exists: Option<PathBuf>,
    #[arg(long, global = true, env = "BRAINTRUST_API_KEY", hide = true)]
    braintrust_api_key: Option<String>,
    #[arg(long, global = true, env = "BRAINTRUST_APP_URL", hide = true)]
    braintrust_app_url: Option<String>,
    #[arg(long, global = true, env = "BRAINTRUST_API_URL", hide = true)]
    braintrust_api_url: Option<String>,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum HarnessKind {
    Basic,
    Rlm,
    #[value(name = "typescript")]
    TypeScript,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum SecretBackendArg {
    #[value(name = "apple-keychain")]
    AppleKeychain,
    File,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum SandboxBackendArg {
    #[value(name = "apple-container")]
    AppleContainer,
    Docker,
    #[value(name = "local-process")]
    LocalProcess,
}

fn build_exo_config(cli: &Cli) -> Result<BasicExoHarnessConfig, Box<dyn std::error::Error>> {
    let secret_backend = match cli.secret_backend.unwrap_or_else(default_secret_backend) {
        SecretBackendArg::AppleKeychain => SecretBackendChoice::AppleKeychain,
        SecretBackendArg::File => SecretBackendChoice::File {
            path: cli.master_key_path.clone(),
        },
    };
    let sandbox_backend = match cli.sandbox_backend.unwrap_or_else(default_sandbox_backend) {
        SandboxBackendArg::AppleContainer => SandboxBackendChoice::AppleContainer,
        SandboxBackendArg::Docker => SandboxBackendChoice::Docker,
        SandboxBackendArg::LocalProcess => SandboxBackendChoice::LocalProcess,
    };
    Ok(BasicExoHarnessConfig {
        root: cli.root.join("exoharness"),
        secret_backend,
        sandbox_backend,
    })
}

#[cfg(target_os = "macos")]
fn default_secret_backend() -> SecretBackendArg {
    SecretBackendArg::AppleKeychain
}

#[cfg(not(target_os = "macos"))]
fn default_secret_backend() -> SecretBackendArg {
    SecretBackendArg::File
}

#[cfg(target_os = "macos")]
fn default_sandbox_backend() -> SandboxBackendArg {
    SandboxBackendArg::AppleContainer
}

#[cfg(not(target_os = "macos"))]
fn default_sandbox_backend() -> SandboxBackendArg {
    SandboxBackendArg::Docker
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum EnabledDisabled {
    Enabled,
    Disabled,
}

impl EnabledDisabled {
    fn enabled(self) -> bool {
        matches!(self, Self::Enabled)
    }
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Start a chat REPL using a registered model, creating a default agent and
    /// conversation if they don't exist yet.
    Repl {
        /// Model binding to use (defaults to the first registered model).
        #[arg(long)]
        model: Option<String>,
        /// Agent slug to use or create (default: "repl").
        #[arg(long)]
        agent: Option<String>,
        /// Conversation slug to use or create (default: "repl").
        #[arg(long)]
        conversation: Option<String>,
    },
    Agent {
        #[command(subcommand)]
        command: AgentCommands,
    },
    Conversation {
        #[command(subcommand)]
        command: ConversationCommands,
    },
    Chat {
        #[command(subcommand)]
        command: ChatCommands,
    },
    Secret {
        #[command(subcommand)]
        command: SecretCommands,
    },
    Model {
        #[command(subcommand)]
        command: ModelCommands,
    },
}

#[derive(Debug, Subcommand)]
enum AgentCommands {
    List,
    Create {
        name: String,
        #[arg(long)]
        slug: Option<String>,
        #[arg(long)]
        module: Option<PathBuf>,
        #[arg(long = "tool-manifest")]
        tool_manifests: Vec<PathBuf>,
        #[arg(long, value_enum)]
        tool_creation: Option<EnabledDisabled>,
        #[arg(long)]
        sandbox_image: Option<String>,
        #[arg(long, value_enum)]
        networking: Option<EnabledDisabled>,
        #[arg(long)]
        model: String,
        #[arg(long)]
        max_output_tokens: Option<i64>,
        #[arg(long)]
        max_tool_round_trips: Option<u32>,
        #[arg(long)]
        braintrust_org: Option<String>,
        #[arg(long)]
        braintrust_project: Option<String>,
        #[arg(long)]
        braintrust_project_id: Option<String>,
    },
    Update {
        agent: String,
        #[arg(long)]
        set_harness: Option<HarnessKind>,
        #[arg(long)]
        module: Option<PathBuf>,
        #[arg(long)]
        clear_module: bool,
        #[arg(long = "tool-manifest")]
        tool_manifests: Vec<PathBuf>,
        #[arg(long = "clear-tool-manifests")]
        clear_tool_manifests: bool,
        #[arg(long, value_enum)]
        tool_creation: Option<EnabledDisabled>,
        #[arg(long)]
        sandbox_image: Option<String>,
        #[arg(long)]
        clear_sandbox_image: bool,
        #[arg(long, value_enum)]
        networking: Option<EnabledDisabled>,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        max_output_tokens: Option<i64>,
        #[arg(long)]
        clear_max_output_tokens: bool,
        #[arg(long)]
        max_tool_round_trips: Option<u32>,
        #[arg(long)]
        clear_max_tool_round_trips: bool,
        #[arg(long)]
        clear_braintrust: bool,
        #[arg(long)]
        braintrust_org: Option<String>,
        #[arg(long)]
        braintrust_project: Option<String>,
        #[arg(long)]
        braintrust_project_id: Option<String>,
    },
    Show {
        agent: String,
    },
    Delete {
        agent: String,
    },
}

#[derive(Debug, Subcommand)]
enum ConversationCommands {
    List {
        agent: String,
    },
    Create {
        agent: String,
        name: Option<String>,
        #[arg(long)]
        slug: Option<String>,
        #[arg(long)]
        repl: bool,
    },
    Fork {
        agent: String,
        conversation: String,
        name: Option<String>,
        #[arg(long)]
        slug: Option<String>,
        #[arg(long)]
        up_to: Option<String>,
        #[arg(long)]
        repl: bool,
    },
    Update {
        agent: String,
        conversation: String,
        #[arg(long)]
        shell_program: Option<String>,
        #[arg(long)]
        clear_shell_program: bool,
        #[arg(long, value_enum)]
        networking: Option<EnabledDisabled>,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        max_output_tokens: Option<i64>,
        #[arg(long)]
        clear_max_output_tokens: bool,
        #[arg(long)]
        clear_model_override: bool,
    },
    Mount {
        #[command(subcommand)]
        command: ConversationMountCommands,
    },
    Show {
        agent: String,
        conversation: String,
    },
    Events {
        agent: String,
        conversation: String,
        #[arg(long = "type")]
        types: Vec<String>,
        #[arg(long)]
        limit: Option<u32>,
        #[arg(long)]
        desc: bool,
        #[arg(long)]
        cursor: Option<String>,
        #[arg(long)]
        session_id: Option<String>,
        #[arg(long)]
        turn_id: Option<String>,
    },
    Delete {
        agent: String,
        conversation: String,
    },
}

#[derive(Debug, Subcommand)]
enum SecretCommands {
    List,
    Set {
        name: String,
        #[arg(long)]
        env: Option<String>,
        #[arg(long)]
        value: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
enum ModelCommands {
    List,
    Register {
        name: String,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        secret: String,
        #[arg(long)]
        base_url: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
enum ConversationMountCommands {
    List {
        agent: String,
        conversation: String,
    },
    Add {
        agent: String,
        conversation: String,
        host_path: PathBuf,
        mount_path: Option<String>,
        #[arg(long)]
        rw: bool,
        #[arg(long)]
        internal: bool,
    },
    Remove {
        agent: String,
        conversation: String,
        mount_path: String,
    },
}

#[derive(Debug, Subcommand)]
enum ChatCommands {
    Send {
        agent: String,
        conversation: String,
        prompt: String,
    },
    Repl {
        agent: String,
        conversation: String,
    },
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    let exo_config = build_exo_config(&cli)?;
    let env = CliEnvironment::load(cli.env_file_if_exists.as_deref(), cli.env_file.as_deref())?;
    let runtime_config = env.braintrust_runtime_config(
        cli.braintrust_api_key,
        cli.braintrust_app_url,
        cli.braintrust_api_url,
    );
    let env_vars = env.into_vars();
    let harness_kind = determine_harness_kind(&exo_config, cli.harness, &cli.command).await?;
    let harness = instantiate_harness(
        &exo_config,
        harness_kind,
        runtime_config.clone(),
        env_vars.clone(),
    )
    .await?;

    match cli.command {
        Commands::Repl {
            model,
            agent,
            conversation,
        } => {
            let agent_slug = agent.unwrap_or_else(|| DEFAULT_REPL_SLUG.to_string());
            // Without --conversation, start a fresh session each run (the usual CLI
            // behavior); pass --conversation <slug> to resume or target a specific one.
            let conversation_slug = conversation.unwrap_or_else(generate_fun_slug);

            let agent = match harness.get_agent(&agent_slug).await? {
                Some(agent) => agent,
                None => {
                    let model = ensure_repl_model(harness.as_ref(), model).await?;
                    harness
                        .create_agent(CreateAgentRequest {
                            slug: agent_slug.clone(),
                            name: Some(agent_slug),
                            harness: to_agent_harness_kind(harness_kind),
                            typescript: None,
                            enable_agent_tool_creation: true,
                            sandbox_image: None,
                            enable_networking: false,
                            model,
                            max_output_tokens: None,
                            max_tool_round_trips: None,
                            braintrust: None,
                        })
                        .await?
                }
            };

            let conversation = match agent.get_conversation(&conversation_slug).await? {
                Some(conversation) => conversation,
                None => {
                    let conversation = agent
                        .create_conversation(CreateConversationRequest {
                            slug: Some(conversation_slug.clone()),
                            name: Some(conversation_slug),
                        })
                        .await?;
                    // The default REPL is a plain chat: drop the shell tool so no
                    // sandbox is provisioned. Use a regular conversation for tools.
                    let mut config = conversation.config().await?;
                    if config.shell_program.is_some() {
                        config.shell_program = None;
                        conversation.put_config(config).await?;
                    }
                    conversation
                }
            };

            run_chat_repl(conversation).await?;
        }
        Commands::Agent { command } => match command {
            AgentCommands::List => {
                println!("AGENT\tNAME");
                for agent in harness.list_agents().await? {
                    println!("{}\t{}", agent.slug, agent.name);
                }
            }
            AgentCommands::Create {
                name,
                slug,
                module,
                tool_manifests,
                tool_creation,
                sandbox_image,
                networking,
                model,
                max_output_tokens,
                max_tool_round_trips,
                braintrust_org,
                braintrust_project,
                braintrust_project_id,
            } => {
                let slug = slug.unwrap_or_else(|| slugify(&name));
                if slug.is_empty() {
                    return Err("agent slug resolved to an empty value".into());
                }
                if sandbox_image
                    .as_ref()
                    .is_some_and(|image| image.trim().is_empty())
                {
                    return Err("sandbox image must not be empty".into());
                }
                let agent_harness_kind = to_agent_harness_kind(harness_kind);
                let typescript = build_typescript_harness_config(
                    harness_kind,
                    module.as_deref(),
                    &tool_manifests,
                )?;
                let agent = harness
                    .create_agent(CreateAgentRequest {
                        slug,
                        name: Some(name),
                        harness: agent_harness_kind,
                        typescript,
                        enable_agent_tool_creation: tool_creation
                            .map(EnabledDisabled::enabled)
                            .unwrap_or(true),
                        sandbox_image,
                        enable_networking: networking.is_some_and(EnabledDisabled::enabled),
                        model,
                        max_output_tokens,
                        max_tool_round_trips,
                        braintrust: build_braintrust_tracing_config(
                            braintrust_org,
                            braintrust_project,
                            braintrust_project_id,
                        )?,
                    })
                    .await?;
                println!(
                    "created agent {} ({})",
                    agent.record().slug,
                    agent.record().id
                );
            }
            AgentCommands::Update {
                agent,
                set_harness,
                module,
                clear_module,
                tool_manifests,
                clear_tool_manifests,
                tool_creation,
                sandbox_image,
                clear_sandbox_image,
                networking,
                model,
                max_output_tokens,
                clear_max_output_tokens,
                max_tool_round_trips,
                clear_max_tool_round_trips,
                clear_braintrust,
                braintrust_org,
                braintrust_project,
                braintrust_project_id,
            } => {
                if clear_module && module.is_some() {
                    return Err("provide either --clear-module or --module, not both".into());
                }
                if clear_tool_manifests && !tool_manifests.is_empty() {
                    return Err(
                        "provide either --clear-tool-manifests or --tool-manifest, not both".into(),
                    );
                }
                if clear_module && !tool_manifests.is_empty() {
                    return Err("provide either --clear-module or --tool-manifest, not both".into());
                }
                if clear_sandbox_image && sandbox_image.is_some() {
                    return Err(
                        "provide either --clear-sandbox-image or --sandbox-image, not both".into(),
                    );
                }
                if clear_max_output_tokens && max_output_tokens.is_some() {
                    return Err(
                        "provide either --clear-max-output-tokens or --max-output-tokens, not both"
                            .into(),
                    );
                }
                if clear_max_tool_round_trips && max_tool_round_trips.is_some() {
                    return Err(
                        "provide either --clear-max-tool-round-trips or --max-tool-round-trips, not both"
                            .into(),
                    );
                }
                if clear_braintrust
                    && (braintrust_org.is_some()
                        || braintrust_project.is_some()
                        || braintrust_project_id.is_some())
                {
                    return Err(
                        "provide either --clear-braintrust or Braintrust project flags, not both"
                            .into(),
                    );
                }
                let agent = must_get_agent(harness.as_ref(), &agent).await?;
                let mut config = agent.config().await?;
                let mut changed = false;
                if let Some(set_harness) = set_harness {
                    let new_harness = to_agent_harness_kind(set_harness);
                    if config.harness != new_harness {
                        config.harness = new_harness;
                        changed = true;
                    }
                }
                if clear_module {
                    config.typescript = None;
                    changed = true;
                } else if let Some(module) = module.as_deref() {
                    if config.harness != AgentHarnessKind::TypeScript {
                        return Err("--module is only valid with TypeScript agents".into());
                    }
                    let existing_tool_modules = config
                        .typescript
                        .as_ref()
                        .map(|typescript| typescript.tool_module_paths.clone())
                        .unwrap_or_default();
                    let tool_module_paths = if tool_manifests.is_empty() {
                        existing_tool_modules
                    } else {
                        resolve_typescript_tool_module_paths(&tool_manifests)?
                    };
                    let typescript = Some(resolve_typescript_harness_config(
                        module,
                        tool_module_paths,
                    )?);
                    if config.typescript != typescript {
                        config.typescript = typescript;
                        changed = true;
                    }
                } else if clear_tool_manifests {
                    if let Some(typescript) = config.typescript.as_mut()
                        && !typescript.tool_module_paths.is_empty()
                    {
                        typescript.tool_module_paths.clear();
                        changed = true;
                    }
                } else if !tool_manifests.is_empty() {
                    if config.harness != AgentHarnessKind::TypeScript {
                        return Err("--tool-manifest is only valid with TypeScript agents".into());
                    }
                    let Some(typescript) = config.typescript.as_mut() else {
                        return Err(
                            "typescript agents require a module path; pass --module <path>".into(),
                        );
                    };
                    let tool_module_paths = resolve_typescript_tool_module_paths(&tool_manifests)?;
                    if typescript.tool_module_paths != tool_module_paths {
                        typescript.tool_module_paths = tool_module_paths;
                        changed = true;
                    }
                }
                if let Some(tool_creation) = tool_creation {
                    let enable_agent_tool_creation = tool_creation.enabled();
                    if config.enable_agent_tool_creation != enable_agent_tool_creation {
                        config.enable_agent_tool_creation = enable_agent_tool_creation;
                        changed = true;
                    }
                }
                if clear_sandbox_image {
                    config.sandbox_image = None;
                    changed = true;
                } else if let Some(sandbox_image) = sandbox_image {
                    if sandbox_image.trim().is_empty() {
                        return Err("sandbox image must not be empty".into());
                    }
                    config.sandbox_image = Some(sandbox_image);
                    changed = true;
                }

                if let Some(networking) = networking {
                    let enable_networking = networking.enabled();
                    if config.enable_networking != enable_networking {
                        config.enable_networking = enable_networking;
                        changed = true;
                    }
                }

                if let Some(model) = model {
                    if model.trim().is_empty() {
                        return Err("model must not be empty".into());
                    }
                    if config.model != model {
                        config.model = model;
                        changed = true;
                    }
                }
                if clear_max_output_tokens {
                    if config.max_output_tokens.is_some() {
                        config.max_output_tokens = None;
                        changed = true;
                    }
                } else if let Some(max_output_tokens) = max_output_tokens
                    && config.max_output_tokens != Some(max_output_tokens)
                {
                    config.max_output_tokens = Some(max_output_tokens);
                    changed = true;
                }
                if clear_max_tool_round_trips {
                    if config.max_tool_round_trips.is_some() {
                        config.max_tool_round_trips = None;
                        changed = true;
                    }
                } else if let Some(max_tool_round_trips) = max_tool_round_trips
                    && config.max_tool_round_trips != Some(max_tool_round_trips)
                {
                    config.max_tool_round_trips = Some(max_tool_round_trips);
                    changed = true;
                }

                if clear_braintrust {
                    config.braintrust = None;
                    changed = true;
                } else {
                    let updated_braintrust = build_braintrust_tracing_config(
                        braintrust_org,
                        braintrust_project,
                        braintrust_project_id,
                    )?;
                    if updated_braintrust.is_none() && !changed {
                        return Err(
                            "no changes provided; pass --set-harness, --module, --tool-manifest, --clear-tool-manifests, --tool-creation, --sandbox-image, --networking, model flags, --clear-braintrust, or Braintrust project flags"
                                .into(),
                        );
                    }
                    if updated_braintrust.is_some() {
                        config.braintrust = updated_braintrust;
                        changed = true;
                    }
                }
                if !changed {
                    return Err("no changes provided".into());
                }
                if config.harness == AgentHarnessKind::TypeScript && config.typescript.is_none() {
                    return Err(
                        "typescript agents require a module path; pass --module <path>".into(),
                    );
                }
                agent.put_config(config).await?;
                println!("updated agent {}", agent.record().slug);
            }
            AgentCommands::Show { agent } => {
                let agent = must_get_agent(harness.as_ref(), &agent).await?;
                let config = agent.config().await?;
                println!("id: {}", agent.record().id);
                println!("slug: {}", agent.record().slug);
                println!("name: {}", agent.record().name);
                println!("harness: {}", format_harness_kind(config.harness));
                println!(
                    "typescript_module: {}",
                    config
                        .typescript
                        .as_ref()
                        .map(|config| config.module_path.as_str())
                        .unwrap_or("none")
                );
                let tool_module_paths = config
                    .typescript
                    .as_ref()
                    .map(|config| config.tool_module_paths.as_slice())
                    .unwrap_or_default();
                println!("typescript_tool_manifests: {}", tool_module_paths.len());
                for tool_module_path in tool_module_paths {
                    println!("  - {}", tool_module_path);
                }
                println!(
                    "tool_creation: {}",
                    if config.enable_agent_tool_creation {
                        "enabled"
                    } else {
                        "disabled"
                    }
                );
                println!(
                    "sandbox_image: {}",
                    config.sandbox_image.as_deref().unwrap_or("default")
                );
                println!("enable_networking: {}", config.enable_networking);
                println!("model: {}", config.model);
                println!(
                    "max_output_tokens: {}",
                    config
                        .max_output_tokens
                        .map(|value| value.to_string())
                        .unwrap_or_else(|| "none".to_string())
                );
                println!(
                    "max_tool_round_trips: {}",
                    config
                        .max_tool_round_trips
                        .map(|value| value.to_string())
                        .unwrap_or_else(|| "none".to_string())
                );
                println!(
                    "braintrust: {}",
                    format_braintrust_tracing_config(config.braintrust.as_ref())
                );
            }
            AgentCommands::Delete { agent } => {
                if !harness.delete_agent(&agent).await? {
                    return Err(format!("agent not found: {agent}").into());
                }
                println!("deleted agent {}", agent);
            }
        },
        Commands::Conversation { command } => match command {
            ConversationCommands::List { agent } => {
                let agent = must_get_agent(harness.as_ref(), &agent).await?;
                println!("CONVERSATION\tNAME");
                for conversation in agent.list_conversations().await? {
                    println!("{}\t{}", conversation.slug, conversation.name);
                }
            }
            ConversationCommands::Create {
                agent,
                name,
                slug,
                repl,
            } => {
                let agent = must_get_agent(harness.as_ref(), &agent).await?;
                let slug = slug.unwrap_or_else(|| {
                    name.as_deref()
                        .map(slugify)
                        .filter(|slug| !slug.is_empty())
                        .unwrap_or_else(generate_fun_slug)
                });
                if slug.is_empty() {
                    return Err("conversation slug resolved to an empty value".into());
                }
                let conversation = agent
                    .create_conversation(CreateConversationRequest {
                        slug: Some(slug),
                        name,
                    })
                    .await?;
                println!(
                    "created conversation {} ({})",
                    conversation.record().slug,
                    conversation.record().id
                );
                if repl {
                    run_chat_repl(conversation).await?;
                } else {
                    println!(
                        "start chatting with it via `{}`",
                        chat_repl_command(
                            agent.record().slug.as_str(),
                            conversation.record().slug.as_str(),
                        )
                    );
                }
            }
            ConversationCommands::Fork {
                agent,
                conversation,
                name,
                slug,
                up_to,
                repl,
            } => {
                let source = must_get_conversation(harness.as_ref(), &agent, &conversation).await?;
                let forked = source
                    .exoharness_handle()
                    .fork(ForkConversationRequest {
                        up_to_inclusive: parse_optional_uuid7(up_to.as_deref(), "up_to")?,
                        slug,
                        name,
                    })
                    .await?;
                println!(
                    "forked conversation {} ({})",
                    forked.record().slug,
                    forked.record().id
                );
                if repl {
                    let agent = must_get_agent(harness.as_ref(), &agent).await?;
                    let conversation = agent
                        .get_conversation(&forked.record().slug)
                        .await?
                        .ok_or_else(|| {
                            format!("forked conversation not found: {}", forked.record().slug)
                        })?;
                    run_chat_repl(conversation).await?;
                } else {
                    println!(
                        "start chatting with it via `{}`",
                        chat_repl_command(agent.as_str(), forked.record().slug.as_str())
                    );
                }
            }
            ConversationCommands::Update {
                agent,
                conversation,
                shell_program,
                clear_shell_program,
                networking,
                model,
                max_output_tokens,
                clear_max_output_tokens,
                clear_model_override,
            } => {
                if clear_shell_program && shell_program.is_some() {
                    return Err(
                        "provide either --clear-shell-program or --shell-program, not both".into(),
                    );
                }
                if clear_model_override
                    && (model.is_some() || max_output_tokens.is_some() || clear_max_output_tokens)
                {
                    return Err(
                        "provide either --clear-model-override or model override flags, not both"
                            .into(),
                    );
                }
                if clear_max_output_tokens && max_output_tokens.is_some() {
                    return Err(
                        "provide either --clear-max-output-tokens or --max-output-tokens, not both"
                            .into(),
                    );
                }

                let agent_handle = must_get_agent(harness.as_ref(), &agent).await?;
                let conversation = agent_handle
                    .get_conversation(&conversation)
                    .await?
                    .ok_or_else(|| format!("conversation not found: {}", conversation))?;
                let mut config = conversation.config().await?;
                let mut changed = false;

                if clear_shell_program {
                    config.shell_program = None;
                    changed = true;
                } else if let Some(shell_program) = shell_program {
                    if shell_program.trim().is_empty() {
                        return Err("shell program must not be empty".into());
                    }
                    config.shell_program = Some(shell_program);
                    changed = true;
                }

                if let Some(networking) = networking {
                    config.enable_networking = networking.enabled();
                    changed = true;
                }

                let updated_model_override = if clear_model_override {
                    changed = true;
                    Some(None)
                } else if model.is_some() || max_output_tokens.is_some() || clear_max_output_tokens
                {
                    let agent_config = agent_handle.config().await?;
                    let mut model_override =
                        conversation
                            .model_override()
                            .await?
                            .unwrap_or(ConversationModelConfig {
                                model: agent_config.model,
                                max_output_tokens: agent_config.max_output_tokens,
                            });

                    if let Some(model) = model {
                        if model.trim().is_empty() {
                            return Err("model must not be empty".into());
                        }
                        model_override.model = model;
                    }
                    if clear_max_output_tokens {
                        model_override.max_output_tokens = None;
                    } else if let Some(max_output_tokens) = max_output_tokens {
                        model_override.max_output_tokens = Some(max_output_tokens);
                    }

                    changed = true;
                    Some(Some(model_override))
                } else {
                    None
                };

                if !changed {
                    return Err("no changes provided".into());
                }

                conversation.put_config(config).await?;
                if let Some(model_override) = updated_model_override {
                    conversation.put_model_override(model_override).await?;
                }
                println!("updated conversation {}", conversation.record().slug);
            }
            ConversationCommands::Mount { command } => match command {
                ConversationMountCommands::List {
                    agent,
                    conversation,
                } => {
                    let conversation =
                        must_get_conversation(harness.as_ref(), &agent, &conversation).await?;
                    let config = conversation.config().await?;
                    print_mounts(&config.mounts);
                }
                ConversationMountCommands::Add {
                    agent,
                    conversation,
                    host_path,
                    mount_path,
                    rw,
                    internal,
                } => {
                    let conversation =
                        must_get_conversation(harness.as_ref(), &agent, &conversation).await?;
                    let canonical_host_path = canonicalize_directory(&host_path)?;

                    let mut config = conversation.config().await?;
                    let mount_path = match mount_path {
                        Some(mount_path) => {
                            validate_mount_path(&mount_path)?;
                            mount_path
                        }
                        None => default_mount_path(&canonical_host_path, &config.mounts),
                    };
                    let new_mount = FileSystemMount {
                        host_path: canonical_host_path.display().to_string(),
                        mount_path: mount_path.clone(),
                        mode: if rw {
                            FileSystemMountMode::ReadWrite
                        } else {
                            FileSystemMountMode::ReadOnly
                        },
                        internal: Some(internal),
                    };

                    if let Some(existing) = config
                        .mounts
                        .iter_mut()
                        .find(|mount| mount.mount_path == mount_path)
                    {
                        *existing = new_mount;
                    } else {
                        config.mounts.push(new_mount);
                    }

                    conversation.put_config(config).await?;
                    println!(
                        "mounted {} -> {} ({}) for {}",
                        canonical_host_path.display(),
                        mount_path,
                        if rw { "rw" } else { "ro" },
                        conversation.record().slug
                    );
                }
                ConversationMountCommands::Remove {
                    agent,
                    conversation,
                    mount_path,
                } => {
                    let conversation =
                        must_get_conversation(harness.as_ref(), &agent, &conversation).await?;
                    let mut config = conversation.config().await?;
                    let before = config.mounts.len();
                    config.mounts.retain(|mount| mount.mount_path != mount_path);
                    if config.mounts.len() == before {
                        return Err(format!("mount not found: {mount_path}").into());
                    }
                    conversation.put_config(config).await?;
                    println!(
                        "removed mount {} from {}",
                        mount_path,
                        conversation.record().slug
                    );
                }
            },
            ConversationCommands::Show {
                agent,
                conversation,
            } => {
                let agent_handle = must_get_agent(harness.as_ref(), &agent).await?;
                let agent_config = agent_handle.config().await?;
                let conversation = agent_handle
                    .get_conversation(&conversation)
                    .await?
                    .ok_or_else(|| format!("conversation not found: {}", conversation))?;
                let config = conversation.config().await?;
                let model_override = conversation.model_override().await?;
                let messages = conversation.messages().await?;
                let effective_model =
                    model_override
                        .clone()
                        .unwrap_or_else(|| ConversationModelConfig {
                            model: agent_config.model.clone(),
                            max_output_tokens: agent_config.max_output_tokens,
                        });
                println!("id: {}", conversation.record().id);
                println!("slug: {}", conversation.record().slug);
                println!("name: {}", conversation.record().name);
                println!(
                    "latest_event_id: {}",
                    conversation
                        .record()
                        .latest_event_id
                        .map(|value| value.to_string())
                        .unwrap_or_else(|| "none".to_string())
                );
                println!("message_count: {}", messages.len());
                println!("enable_networking: {}", config.enable_networking);
                println!(
                    "effective_enable_networking: {}",
                    agent_config.enable_networking || config.enable_networking
                );
                println!(
                    "shell_program: {}",
                    config.shell_program.unwrap_or_else(|| "none".to_string())
                );
                println!(
                    "effective_sandbox_image: {}",
                    agent_config.sandbox_image.as_deref().unwrap_or("default")
                );
                println!(
                    "model_override: {}",
                    model_override
                        .as_ref()
                        .map(|config| config.to_string())
                        .unwrap_or_else(|| "none".to_string())
                );
                println!("effective_model: {}", effective_model.model);
                println!(
                    "effective_max_output_tokens: {}",
                    effective_model
                        .max_output_tokens
                        .map(|value| value.to_string())
                        .unwrap_or_else(|| "none".to_string())
                );
                println!("mounts:");
                print_mounts(&config.mounts);
            }
            ConversationCommands::Events {
                agent,
                conversation,
                types,
                limit,
                desc,
                cursor,
                session_id,
                turn_id,
            } => {
                let conversation =
                    must_get_conversation(harness.as_ref(), &agent, &conversation).await?;
                let result = conversation
                    .exoharness_handle()
                    .get_events(Some(EventQuery {
                        cursor: parse_optional_uuid7(cursor.as_deref(), "cursor")?,
                        direction: Some(if desc {
                            EventQueryDirection::Desc
                        } else {
                            EventQueryDirection::Asc
                        }),
                        limit,
                        session_id: parse_optional_uuid7(session_id.as_deref(), "session_id")?,
                        turn_id: parse_optional_uuid7(turn_id.as_deref(), "turn_id")?,
                        types: if types.is_empty() { None } else { Some(types) },
                    }))
                    .await?;
                println!("{}", serde_json::to_string_pretty(&result)?);
            }
            ConversationCommands::Delete {
                agent,
                conversation,
            } => {
                let agent = must_get_agent(harness.as_ref(), &agent).await?;
                if !agent.delete_conversation(&conversation).await? {
                    return Err(format!("conversation not found: {conversation}").into());
                }
                println!("deleted conversation {}", conversation);
            }
        },
        Commands::Chat { command } => match command {
            ChatCommands::Send {
                agent,
                conversation,
                prompt,
            } => {
                let conversation =
                    must_get_conversation(harness.as_ref(), &agent, &conversation).await?;
                let previous_messages = conversation.messages().await?;
                let result = conversation
                    .send(SendRequest {
                        input: vec![Message::User {
                            content: UserContent::String(prompt),
                        }],
                        session_id: None,
                    })
                    .await?;
                conversation.close_session(result.session_id).await?;
                let messages = conversation.messages().await?;
                for message in &messages[previous_messages.len()..] {
                    print_message(message);
                }
            }
            ChatCommands::Repl {
                agent,
                conversation,
            } => {
                let conversation =
                    must_get_conversation(harness.as_ref(), &agent, &conversation).await?;
                run_chat_repl(conversation).await?;
            }
        },
        Commands::Secret { command } => match command {
            SecretCommands::List => {
                println!("SECRET\tTYPE\tCREATED_AT");
                for secret in harness.exoharness_handle().list_secrets().await? {
                    println!(
                        "{}\t{:?}\t{}",
                        secret.name, secret.r#type, secret.created_at
                    );
                }
            }
            SecretCommands::Set { name, env, value } => {
                let value = match (env, value) {
                    (Some(env), None) => secret_value_from_env_arg(&env, &env_vars)?,
                    (None, Some(value)) => value,
                    (Some(_), Some(_)) => {
                        return Err("provide either --env or --value, not both".into());
                    }
                    (None, None) => return Err("provide --env or --value".into()),
                };
                let id = harness
                    .exoharness_handle()
                    .put_secret(PutSecretRequest {
                        name: name.clone(),
                        secret: Secret::Key { value },
                    })
                    .await?;
                println!("set secret {} ({})", name, id);
            }
        },
        Commands::Model { command } => match command {
            ModelCommands::List => {
                println!("MODEL\tUPSTREAM_MODEL\tSECRET\tBASE_URL");
                for model in list_model_bindings(harness.exoharness_handle().as_ref()).await? {
                    println!(
                        "{}\t{}\t{}\t{}",
                        model.name,
                        model.model,
                        model.secret_name.unwrap_or_else(|| "none".to_string()),
                        model.base_url.unwrap_or_else(|| "default".to_string())
                    );
                }
            }
            ModelCommands::Register {
                name,
                model,
                secret,
                base_url,
            } => {
                let secret_id = find_secret_id(harness.exoharness_handle().as_ref(), &secret)
                    .await?
                    .ok_or_else(|| format!("secret not found: {secret}"))?;
                let upstream_model = model.unwrap_or_else(|| name.clone());
                let id = harness
                    .exoharness_handle()
                    .put_binding(Binding::Llm {
                        name: name.clone(),
                        model: upstream_model,
                        base_url,
                        secret_id: Some(secret_id),
                    })
                    .await?;
                println!("registered model {} ({})", name, id);
            }
        },
    }

    harness.flush_tracing().await?;
    Ok(())
}

async fn determine_harness_kind(
    exo_config: &BasicExoHarnessConfig,
    override_kind: Option<HarnessKind>,
    command: &Commands,
) -> Result<HarnessKind, Box<dyn std::error::Error>> {
    if let Some(kind) = override_kind {
        return Ok(kind);
    }

    let Some(agent_ref) = command_agent_ref(command) else {
        return Ok(HarnessKind::Basic);
    };

    Ok(infer_agent_harness_kind(exo_config, agent_ref)
        .await?
        .unwrap_or(HarnessKind::Basic))
}

fn command_agent_ref(command: &Commands) -> Option<&str> {
    match command {
        Commands::Agent { command } => match command {
            AgentCommands::Update { agent, .. }
            | AgentCommands::Show { agent }
            | AgentCommands::Delete { agent } => Some(agent.as_str()),
            AgentCommands::List | AgentCommands::Create { .. } => None,
        },
        Commands::Conversation { command } => match command {
            ConversationCommands::List { agent }
            | ConversationCommands::Create { agent, .. }
            | ConversationCommands::Fork { agent, .. }
            | ConversationCommands::Update { agent, .. }
            | ConversationCommands::Show { agent, .. }
            | ConversationCommands::Events { agent, .. }
            | ConversationCommands::Delete { agent, .. } => Some(agent.as_str()),
            ConversationCommands::Mount { command } => match command {
                ConversationMountCommands::List { agent, .. }
                | ConversationMountCommands::Add { agent, .. }
                | ConversationMountCommands::Remove { agent, .. } => Some(agent.as_str()),
            },
        },
        Commands::Chat { command } => match command {
            ChatCommands::Send { agent, .. } | ChatCommands::Repl { agent, .. } => {
                Some(agent.as_str())
            }
        },
        Commands::Repl { agent, .. } => Some(agent.as_deref().unwrap_or(DEFAULT_REPL_SLUG)),
        Commands::Secret { .. } | Commands::Model { .. } => None,
    }
}

async fn infer_agent_harness_kind(
    exo_config: &BasicExoHarnessConfig,
    agent_ref: &str,
) -> Result<Option<HarnessKind>, Box<dyn std::error::Error>> {
    let exoharness = BasicExoHarness::new(exo_config.clone()).await?;
    let agent = if let Ok(agent_id) = agent_ref.parse::<Uuid7>() {
        exoharness.get_agent(&agent_id).await?
    } else {
        exoharness
            .list_agents()
            .await?
            .into_iter()
            .find(|agent| agent.record().slug == agent_ref)
    };
    let Some(agent) = agent else {
        return Ok(None);
    };

    let config = load_agent_config(agent.as_ref()).await?;
    Ok(Some(from_agent_harness_kind(config.harness)))
}

async fn instantiate_harness(
    exo_config: &BasicExoHarnessConfig,
    kind: HarnessKind,
    runtime_config: Option<BraintrustRuntimeConfig>,
    env_vars: HashMap<String, String>,
) -> Result<Arc<dyn Harness>, Box<dyn std::error::Error>> {
    let harness: Arc<dyn Harness> = match kind {
        HarnessKind::Basic => {
            Arc::new(BasicHarness::from_config(exo_config.clone(), runtime_config, env_vars).await?)
        }
        HarnessKind::Rlm => {
            Arc::new(RlmHarness::from_config(exo_config.clone(), runtime_config, env_vars).await?)
        }
        HarnessKind::TypeScript => Arc::new(
            TypeScriptHarness::from_config(exo_config.clone(), runtime_config, env_vars).await?,
        ),
    };
    Ok(harness)
}

fn to_agent_harness_kind(kind: HarnessKind) -> AgentHarnessKind {
    match kind {
        HarnessKind::Basic => AgentHarnessKind::Basic,
        HarnessKind::Rlm => AgentHarnessKind::Rlm,
        HarnessKind::TypeScript => AgentHarnessKind::TypeScript,
    }
}

fn from_agent_harness_kind(kind: AgentHarnessKind) -> HarnessKind {
    match kind {
        AgentHarnessKind::Basic => HarnessKind::Basic,
        AgentHarnessKind::Rlm => HarnessKind::Rlm,
        AgentHarnessKind::TypeScript => HarnessKind::TypeScript,
    }
}

fn format_harness_kind(kind: AgentHarnessKind) -> &'static str {
    match kind {
        AgentHarnessKind::Basic => "basic",
        AgentHarnessKind::Rlm => "rlm",
        AgentHarnessKind::TypeScript => "typescript",
    }
}

fn build_typescript_harness_config(
    harness_kind: HarnessKind,
    module: Option<&Path>,
    tool_manifests: &[PathBuf],
) -> Result<Option<TypeScriptHarnessConfig>, Box<dyn std::error::Error>> {
    if !matches!(harness_kind, HarnessKind::TypeScript) && !tool_manifests.is_empty() {
        return Err("--tool-manifest is only valid with --harness typescript".into());
    }
    match (harness_kind, module) {
        (HarnessKind::TypeScript, Some(module)) => Ok(Some(resolve_typescript_harness_config(
            module,
            resolve_typescript_tool_module_paths(tool_manifests)?,
        )?)),
        (HarnessKind::TypeScript, None) => Err("typescript agents require --module <path>".into()),
        (_, Some(_)) => Err("--module is only valid with --harness typescript".into()),
        (_, None) => Ok(None),
    }
}

fn resolve_typescript_harness_config(
    module_path: &Path,
    tool_module_paths: Vec<String>,
) -> Result<TypeScriptHarnessConfig, Box<dyn std::error::Error>> {
    let module_path = std::fs::canonicalize(module_path)?;
    Ok(TypeScriptHarnessConfig {
        module_path: module_path.to_string_lossy().into_owned(),
        tool_module_paths,
    })
}

fn resolve_typescript_tool_module_paths(
    paths: &[PathBuf],
) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    paths
        .iter()
        .map(|path| {
            let path = std::fs::canonicalize(path)?;
            Ok(path.to_string_lossy().into_owned())
        })
        .collect()
}

struct RegisteredModel {
    name: String,
    model: String,
    secret_name: Option<String>,
    base_url: Option<String>,
}

async fn list_model_bindings(
    exoharness: &dyn ExoHarness,
) -> Result<Vec<RegisteredModel>, Box<dyn std::error::Error>> {
    let secrets = exoharness.list_secrets().await?;
    let mut models = Vec::new();
    for metadata in exoharness.list_bindings().await? {
        let Some(Binding::Llm {
            name,
            model,
            base_url,
            secret_id,
        }) = exoharness.get_binding(&metadata.id).await?
        else {
            continue;
        };
        let secret_name = secret_id.and_then(|secret_id| {
            secrets
                .iter()
                .find(|secret| secret.id == secret_id)
                .map(|secret| secret.name.clone())
        });
        models.push(RegisteredModel {
            name,
            model,
            secret_name,
            base_url,
        });
    }
    Ok(models)
}

const DEFAULT_REPL_SLUG: &str = "repl";

/// Resolves the model binding a quickstart REPL agent should use. Registering a
/// model is left to `exo secret set` / `exo model register`, so the substrate
/// never reads credentials from the environment on its own.
async fn ensure_repl_model(
    harness: &dyn Harness,
    requested: Option<String>,
) -> Result<String, Box<dyn std::error::Error>> {
    let registered: Vec<String> = list_model_bindings(harness.exoharness_handle().as_ref())
        .await?
        .into_iter()
        .map(|binding| binding.name)
        .collect();
    pick_repl_model(&registered, requested)
}

/// Picks the model an explicit request names, falling back to the first
/// registered binding. Errors with setup guidance when neither is available.
fn pick_repl_model(
    registered: &[String],
    requested: Option<String>,
) -> Result<String, Box<dyn std::error::Error>> {
    if let Some(requested) = requested {
        if registered.iter().any(|name| name == &requested) {
            return Ok(requested);
        }
        return Err(format!(
            "model is not registered: {requested}; register it with `exo model register {requested} --secret <secret>`"
        )
        .into());
    }
    registered.first().cloned().ok_or_else(|| {
        "no model is registered; set one up first:\n  \
         exo secret set openai --env OPENAI_API_KEY\n  \
         exo model register gpt-5.5 --secret openai"
            .into()
    })
}

async fn find_secret_id(
    exoharness: &dyn ExoHarness,
    name: &str,
) -> Result<Option<Uuid7>, Box<dyn std::error::Error>> {
    Ok(exoharness
        .list_secrets()
        .await?
        .into_iter()
        .find(|secret| secret.name == name)
        .map(|secret| secret.id))
}

fn build_braintrust_tracing_config(
    org_name: Option<String>,
    project_name: Option<String>,
    project_id: Option<String>,
) -> Result<Option<BraintrustTracingConfig>, String> {
    match (project_name, project_id) {
        (Some(_), Some(_)) => Err(
            "provide either --braintrust-project or --braintrust-project-id, not both".to_string(),
        ),
        (Some(project_name), None) => Ok(Some(BraintrustTracingConfig {
            org_name,
            project: BraintrustProject::Name(project_name),
        })),
        (None, Some(project_id)) => Ok(Some(BraintrustTracingConfig {
            org_name,
            project: BraintrustProject::Id(project_id),
        })),
        (None, None) => Ok(None),
    }
}

fn format_braintrust_tracing_config(config: Option<&BraintrustTracingConfig>) -> String {
    let Some(config) = config else {
        return "none".to_string();
    };

    let project = match &config.project {
        BraintrustProject::Name(name) => format!("project={name}"),
        BraintrustProject::Id(id) => format!("project_id={id}"),
    };

    match &config.org_name {
        Some(org_name) => format!("org={org_name}, {project}"),
        None => project,
    }
}

fn parse_optional_uuid7(
    value: Option<&str>,
    field: &str,
) -> Result<Option<Uuid7>, Box<dyn std::error::Error>> {
    match value {
        Some(value) => Ok(Some(
            value
                .parse::<Uuid7>()
                .map_err(|error| format!("invalid {field}: {error}"))?,
        )),
        None => Ok(None),
    }
}

fn canonicalize_directory(path: &PathBuf) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let canonical = std::fs::canonicalize(path)?;
    if !canonical.is_dir() {
        return Err(format!(
            "mount host path is not a directory: {}",
            canonical.display()
        )
        .into());
    }
    Ok(canonical)
}

fn validate_mount_path(mount_path: &str) -> Result<(), Box<dyn std::error::Error>> {
    if mount_path.trim().is_empty() {
        return Err("mount path must not be empty".into());
    }
    if !mount_path.starts_with('/') {
        return Err("mount path must be absolute".into());
    }
    Ok(())
}

pub(crate) fn default_mount_path(host_path: &Path, existing_mounts: &[FileSystemMount]) -> String {
    let base_name = host_path
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("mount");

    let mut candidate = format!("{SANDBOX_MAIN_MOUNT_DIR}/{base_name}");
    let mut suffix = 2;
    while existing_mounts
        .iter()
        .any(|mount| mount.mount_path == candidate)
    {
        candidate = format!("{SANDBOX_MAIN_MOUNT_DIR}/{base_name}-{suffix}");
        suffix += 1;
    }
    candidate
}

fn print_mounts(mounts: &[FileSystemMount]) {
    if mounts.is_empty() {
        println!("  none");
        return;
    }

    for mount in mounts {
        let mode = match mount.mode {
            FileSystemMountMode::ReadOnly => "ro",
            FileSystemMountMode::ReadWrite => "rw",
        };
        let internal = mount.internal.unwrap_or(false);
        println!(
            "  {} -> {} ({mode}{})",
            mount.host_path,
            mount.mount_path,
            if internal { ", internal" } else { "" }
        );
    }
}

async fn must_get_agent(
    harness: &dyn Harness,
    agent_ref: &str,
) -> Result<Arc<dyn HarnessAgent>, Box<dyn std::error::Error>> {
    harness
        .get_agent(agent_ref)
        .await?
        .ok_or_else(|| format!("agent not found: {agent_ref}").into())
}

async fn must_get_conversation(
    harness: &dyn Harness,
    agent_ref: &str,
    conversation_ref: &str,
) -> Result<Arc<dyn HarnessConversation>, Box<dyn std::error::Error>> {
    let agent = must_get_agent(harness, agent_ref).await?;
    agent
        .get_conversation(conversation_ref)
        .await?
        .ok_or_else(|| format!("conversation not found: {conversation_ref}").into())
}

pub(crate) fn print_message(message: &Message) {
    match message {
        Message::User { content } => {
            println!("user: {}", render_user_content(content));
        }
        Message::Assistant { content, .. } => {
            println!("assistant: {}", render_assistant_content(content));
        }
        Message::Tool { content } => {
            for part in content {
                let ToolContentPart::ToolResult(result) = part;
                println!("tool {}: {}", result.tool_name, result.output);
            }
        }
        Message::System { content } => {
            println!("system: {}", render_user_content(content));
        }
        Message::Developer { content } => {
            println!("developer: {}", render_user_content(content));
        }
    }
}

fn render_user_content(content: &UserContent) -> String {
    match content {
        UserContent::String(text) => text.clone(),
        UserContent::Array(parts) => parts
            .iter()
            .map(|part| match part {
                lingua::universal::UserContentPart::Text(text) => text.text.clone(),
                _ => "[non-text user content]".to_string(),
            })
            .collect::<Vec<_>>()
            .join(""),
    }
}

pub(crate) fn render_assistant_content(content: &AssistantContent) -> String {
    match content {
        AssistantContent::String(text) => text.clone(),
        AssistantContent::Array(parts) => parts
            .iter()
            .map(|part| match part {
                AssistantContentPart::Text(text) => text.text.clone(),
                AssistantContentPart::Reasoning { text, .. } => format!("[reasoning] {text}"),
                AssistantContentPart::ToolCall {
                    tool_name,
                    arguments,
                    ..
                } => format!("[tool_call {tool_name}] {arguments}"),
                AssistantContentPart::ToolResult {
                    tool_name, output, ..
                } => format!("[tool_result {tool_name}] {output}"),
                AssistantContentPart::File { .. } => "[file]".to_string(),
            })
            .collect::<Vec<_>>()
            .join(""),
    }
}

fn chat_repl_command(agent_slug: &str, conversation_slug: &str) -> String {
    format!("exo chat repl {agent_slug} {conversation_slug}")
}

fn slugify(input: &str) -> String {
    let mut slug = String::new();
    let mut last_was_dash = false;

    for ch in input.chars() {
        let lower = ch.to_ascii_lowercase();
        if lower.is_ascii_alphanumeric() {
            slug.push(lower);
            last_was_dash = false;
        } else if !last_was_dash && !slug.is_empty() {
            slug.push('-');
            last_was_dash = true;
        }
    }

    while slug.ends_with('-') {
        slug.pop();
    }

    slug
}

pub(crate) fn secret_value_from_env_arg(
    env: &str,
    loaded_env: &HashMap<String, String>,
) -> Result<String, String> {
    if !is_env_var_name(env) {
        return Err(
            "invalid --env value; pass an environment variable name such as OPENAI_API_KEY, not the secret value"
                .to_string(),
        );
    }

    loaded_env
        .get(env)
        .cloned()
        .or_else(|| std::env::var(env).ok())
        .ok_or_else(|| "environment variable passed to --env is not set".to_string())
}

fn is_env_var_name(env: &str) -> bool {
    let mut chars = env.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first == '_' || first.is_ascii_alphabetic()) {
        return false;
    }

    chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

const SLUG_WORDS_A: &[&str] = &[
    "amber", "aster", "basil", "cedar", "cinder", "cobalt", "ember", "fable", "glacier", "harbor",
    "ivy", "juniper", "lilac", "marble", "north", "onyx", "pony", "quartz", "river", "solstice",
    "topaz", "velvet", "willow", "yarrow",
];

const SLUG_WORDS_B: &[&str] = &[
    "anchor", "beacon", "bloom", "cadence", "canyon", "drift", "echo", "feather", "forge",
    "gesture", "grove", "harvest", "lagoon", "lantern", "meadow", "orbit", "passage", "pebble",
    "ridge", "signal", "sparrow", "summit", "thistle", "window",
];

fn generate_fun_slug() -> String {
    generate_fun_slug_from_uuid(Uuid7::now())
}

pub(crate) fn generate_fun_slug_from_uuid(uuid: Uuid7) -> String {
    let bytes = uuid.0.as_bytes();
    let word_a = SLUG_WORDS_A[(bytes[0] as usize) % SLUG_WORDS_A.len()];
    let word_b = SLUG_WORDS_B[(bytes[1] as usize) % SLUG_WORDS_B.len()];
    let suffix = format!("{:02x}{:02x}", bytes[14], bytes[15]);
    format!("{word_a}-{word_b}-{suffix}")
}

#[cfg(test)]
mod create_tests {
    use super::chat_repl_command;

    #[test]
    fn chat_repl_command_uses_agent_and_conversation_slugs() {
        assert_eq!(
            chat_repl_command("rlm", "aster-lantern-47db"),
            "exo chat repl rlm aster-lantern-47db"
        );
    }

    #[test]
    fn repl_command_parses_without_arguments() {
        use clap::Parser;
        let cli = super::Cli::try_parse_from(["exo", "repl"]).expect("repl parses with no args");
        assert!(matches!(
            cli.command,
            super::Commands::Repl {
                model: None,
                agent: None,
                conversation: None,
            }
        ));
    }

    #[test]
    fn repl_command_accepts_overrides() {
        use clap::Parser;
        let cli = super::Cli::try_parse_from(["exo", "repl", "--model", "gpt-5.4"])
            .expect("repl parses with --model");
        assert!(matches!(
            cli.command,
            super::Commands::Repl { model: Some(model), .. } if model == "gpt-5.4"
        ));
    }
}
