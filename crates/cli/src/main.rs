use exoharness::vault::SecretReference;
mod env;
#[cfg(test)]
mod env_tests;
mod environment;
mod managed_agents;
#[cfg(test)]
mod mount_tests;
#[cfg(test)]
mod naming_tests;
mod oauth;
mod providers;
mod render;
#[cfg(test)]
mod secret_tests;
mod serve;
mod tui;
mod tui_app;
mod turn_display;
mod vaults;

use std::collections::HashMap;
use std::io::{self, IsTerminal, Write};
#[cfg(feature = "firecracker")]
use std::net::Ipv4Addr;
#[cfg(feature = "firecracker")]
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};
use executor::{
    AgentHandle, AgentHarnessKind, AttachSandboxRequest, BasicExoHarnessConfig, BasicToolRuntime,
    BraintrustProject, BraintrustTracingConfig, ConversationHandle, ConversationModelConfig,
    CreateConversationRequest, DaytonaBackendSpec, E2bBackendSpec, EventKind, EventQuery,
    EventQueryDirection, ExoHarness, FileSystemMount, FileSystemMountMode, FirecrackerBackendSpec,
    ForkConversationRequest, HOST_EVENT_REBUILD_AND_RESTART, Runtime, SANDBOX_MAIN_MOUNT_DIR,
    SandboxAttachment, SandboxBackendRegistration, SandboxProvider, SandboxScope,
    SecretBackendChoice, SpritesBackendSpec, ToolRequest, ToolRuntime, Uuid7, VercelBackendSpec,
    effective_sandbox_scope, finalize_rebuild_update_file, record_host_event,
    send_conversation_wakeup,
};
use serde::Deserialize;
use tabwriter::TabWriter;
#[cfg(feature = "firecracker")]
use tracing_subscriber::{Layer, layer::SubscriberExt, util::SubscriberInitExt};

#[cfg(feature = "firecracker")]
use executor::{
    DEFAULT_FIRECRACKER_BINARY, DEFAULT_FIRECRACKER_INITRAMFS, DEFAULT_FIRECRACKER_JAILER,
    DEFAULT_FIRECRACKER_KERNEL, DEFAULT_FIRECRACKER_STATE_ROOT, DEFAULT_IMAGE_SIZE_GIB,
    DEFAULT_JAILER_UID_BASE, DEFAULT_NETWORK_BYTES_PER_SECOND, DEFAULT_WORKSPACE_SIZE_GIB,
    FirecrackerConfig, FirecrackerLimaConfig,
};

use crate::env::CliEnvironment;
use crate::render::{Verbosity, print_message};
use executor::managed_agents::TypeScriptHarnessPreset;
use tui::run_chat_repl;

#[derive(Debug, Parser)]
#[command(name = "exo")]
#[command(about = "CLI for exo agents")]
struct Cli {
    /// Override the provider and use its profile context, ignoring directory/global context (saved aliases retain theirs).
    #[arg(long = "provider", global = true)]
    provider_profile: Option<String>,
    /// Directory containing saved provider profiles and authentication state.
    #[arg(long, global = true, env = "EXO_CONFIG_DIR")]
    config_dir: Option<PathBuf>,
    /// Home directory used when --config-dir is not set.
    #[arg(long, global = true, env = "HOME")]
    home: Option<PathBuf>,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Args)]
struct RuntimeArgs {
    /// Directory containing local Exo state.
    #[arg(long, global = true, default_value = ".exo")]
    root: PathBuf,
    /// Store used to protect vault credentials.
    #[arg(long, global = true, value_enum, env = "EXO_SECRET_BACKEND")]
    secret_backend: Option<SecretBackendArg>,
    /// Encryption key for the file secret backend.
    #[arg(long, global = true, env = "EXO_MASTER_KEY_PATH")]
    master_key_path: Option<PathBuf>,
    /// Load environment variables from a file.
    #[arg(long, global = true)]
    env_file: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct ExecutionArgs {
    /// JSON, YAML, or TOML policy for outbound connections and credentials.
    #[arg(long)]
    egress_policy: Option<PathBuf>,
    /// Harness: basic, rlm, exo, typescript, codex, claude-code, cursor, pi, or a TypeScript module path.
    #[arg(long, value_name = "HARNESS")]
    harness: Option<HarnessSelection>,
    #[arg(long, env = "BRAINTRUST_API_KEY", hide = true)]
    braintrust_api_key: Option<String>,
    #[arg(long, env = "BRAINTRUST_APP_URL", hide = true)]
    braintrust_app_url: Option<String>,
    #[arg(long, env = "BRAINTRUST_API_URL", hide = true)]
    braintrust_api_url: Option<String>,
    /// Local LiteLLM price JSON used for cost reporting.
    #[arg(long, env = "EXO_LITELLM_PRICES_PATH")]
    pricing_path: Option<PathBuf>,
    /// URL of the LiteLLM price JSON used for cost reporting.
    #[arg(long, env = "EXO_LITELLM_PRICES_URL")]
    pricing_url: Option<String>,
}

#[cfg(feature = "firecracker")]
#[derive(Debug, Args)]
struct FirecrackerArgs {
    /// Restrict the materializer to these OCI registry entry points. Repeat or
    /// comma-separate values; unset means unrestricted.
    #[arg(
        long = "firecracker-allowed-registry",
        value_name = "REGISTRY",
        value_delimiter = ','
    )]
    allowed_registries: Vec<String>,
    /// Permit these exact local ext4 images as Firecracker roots (repeatable).
    #[arg(long = "firecracker-allowed-local-image", value_name = "PATH")]
    allowed_local_images: Vec<PathBuf>,
    /// Admit these private or special-use IPv4 ranges for guest egress. Repeat
    /// or comma-separate values.
    #[arg(
        long = "firecracker-allowed-egress-cidr",
        value_name = "CIDR",
        value_delimiter = ','
    )]
    allowed_egress_cidrs: Vec<String>,
    /// Maximum number of Firecracker VMs this Exo process may own.
    #[arg(long = "firecracker-max-machines", value_name = "COUNT")]
    max_machines: Option<NonZeroUsize>,
    /// Firecracker VMM executable.
    #[arg(
        long = "firecracker-binary",
        env = "EXO_FIRECRACKER_BINARY",
        default_value = DEFAULT_FIRECRACKER_BINARY
    )]
    firecracker_bin: PathBuf,
    /// Jailer executable matching the Firecracker VMM version.
    #[arg(
        long = "firecracker-jailer",
        env = "EXO_FIRECRACKER_JAILER",
        default_value = DEFAULT_FIRECRACKER_JAILER
    )]
    jailer_bin: PathBuf,
    /// Uncompressed guest kernel image.
    #[arg(
        long = "firecracker-kernel",
        env = "EXO_FIRECRACKER_KERNEL",
        default_value = DEFAULT_FIRECRACKER_KERNEL
    )]
    kernel: PathBuf,
    /// Guest initramfs containing the Exo guest agent.
    #[arg(
        long = "firecracker-initramfs",
        env = "EXO_FIRECRACKER_INITRAMFS",
        default_value = DEFAULT_FIRECRACKER_INITRAMFS
    )]
    initramfs: PathBuf,
    /// Root-owned directory for Firecracker state and jails.
    #[arg(
        long = "firecracker-state-root",
        env = "EXO_FIRECRACKER_STATE_ROOT",
        default_value = DEFAULT_FIRECRACKER_STATE_ROOT
    )]
    state_root: PathBuf,
    /// Maximum materialized OCI root filesystem size, in GiB.
    #[arg(
        long = "firecracker-image-size-gib",
        env = "EXO_FIRECRACKER_IMAGE_SIZE_GIB",
        default_value_t = DEFAULT_IMAGE_SIZE_GIB
    )]
    image_size_gib: u64,
    /// Sparse writable workspace size, in GiB.
    #[arg(
        long = "firecracker-workspace-size-gib",
        env = "EXO_FIRECRACKER_WORKSPACE_SIZE_GIB",
        default_value_t = DEFAULT_WORKSPACE_SIZE_GIB
    )]
    workspace_size_gib: u64,
    /// First host UID/GID in the reserved per-VM jailer range.
    #[arg(
        long = "firecracker-jailer-uid-base",
        env = "EXO_FIRECRACKER_JAILER_UID_BASE",
        default_value_t = DEFAULT_JAILER_UID_BASE
    )]
    jailer_uid_base: u32,
    /// DNS resolver supplied to networking-enabled guests.
    #[arg(
        long = "firecracker-dns-server",
        env = "EXO_FIRECRACKER_DNS_SERVER",
        default_value = "1.1.1.1"
    )]
    dns_server: Ipv4Addr,
    /// Per-VM network rate limit, in bytes per second.
    #[arg(
        long = "firecracker-network-bytes-per-second",
        env = "EXO_FIRECRACKER_NETWORK_BYTES_PER_SECOND",
        default_value_t = DEFAULT_NETWORK_BYTES_PER_SECOND
    )]
    network_bytes_per_second: u64,
    #[cfg(target_os = "macos")]
    /// Lima executable used to manage the outer Firecracker development VM.
    #[arg(
        long = "firecracker-limactl",
        env = "EXO_FIRECRACKER_LIMACTL",
        default_value = "limactl"
    )]
    limactl: PathBuf,
    #[cfg(target_os = "macos")]
    /// Dedicated Lima instance that hosts Firecracker.
    #[arg(
        long = "firecracker-lima-instance",
        env = "EXO_FIRECRACKER_LIMA_INSTANCE",
        default_value = "exo-firecracker"
    )]
    lima_instance: String,
    #[cfg(target_os = "macos")]
    /// Build output directory inside the Lima VM.
    #[arg(
        long = "firecracker-lima-target-dir",
        env = "EXO_FIRECRACKER_LIMA_TARGET_DIR",
        default_value = "/var/tmp/exo-firecracker-bridge-target"
    )]
    lima_target_dir: PathBuf,
    #[cfg(target_os = "macos")]
    /// Prebuilt bridge executable inside Lima; otherwise Exo builds its own.
    #[arg(
        long = "firecracker-lima-exo-binary",
        env = "EXO_FIRECRACKER_LIMA_EXO_BINARY"
    )]
    lima_bridge_binary: Option<PathBuf>,
}

#[cfg(feature = "firecracker")]
impl FirecrackerArgs {
    fn backend_spec(&self) -> Result<FirecrackerBackendSpec> {
        let allowed_egress_cidrs = self
            .allowed_egress_cidrs
            .iter()
            .map(|cidr| {
                cidr.parse()
                    .map_err(|error| anyhow!("invalid Firecracker egress CIDR {cidr}: {error}"))
            })
            .collect::<Result<Vec<_>>>()?;
        let allowed_local_images =
            std::iter::once(PathBuf::from(exoharness::default_firecracker_image()))
                .chain(self.allowed_local_images.iter().cloned())
                .collect();
        let config = FirecrackerConfig {
            egress_listen: None,
            firecracker_bin: self.firecracker_bin.clone(),
            jailer_bin: self.jailer_bin.clone(),
            kernel: self.kernel.clone(),
            initramfs: self.initramfs.clone(),
            state_root: self.state_root.clone(),
            image_size_gib: self.image_size_gib,
            workspace_size_gib: self.workspace_size_gib,
            jailer_uid_base: self.jailer_uid_base,
            dns_server: self.dns_server,
            allowed_egress_cidrs,
            network_device_policy: Default::default(),
            allowed_local_images,
            allowed_registries: self.allowed_registries.clone(),
            network_bytes_per_second: self.network_bytes_per_second,
            max_machines: self.max_machines,
        };
        #[cfg(target_os = "macos")]
        let lima = FirecrackerLimaConfig {
            limactl: self.limactl.clone(),
            instance: self.lima_instance.clone(),
            target_dir: self.lima_target_dir.clone(),
            bridge_binary: self.lima_bridge_binary.clone(),
        };
        #[cfg(not(target_os = "macos"))]
        let lima = FirecrackerLimaConfig::default();
        Ok(FirecrackerBackendSpec { config, lima })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum HarnessKind {
    Basic,
    Rlm,
    #[value(name = "typescript")]
    TypeScript,
    Exo,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum HarnessSelection {
    Kind(HarnessKind),
    TypeScriptPreset(TypeScriptHarnessPreset),
    TypeScriptModule(PathBuf),
}

impl HarnessSelection {
    fn harness_kind(&self) -> HarnessKind {
        match self {
            Self::Kind(kind) => *kind,
            Self::TypeScriptPreset(_) | Self::TypeScriptModule(_) => HarnessKind::TypeScript,
        }
    }
}

impl FromStr for HarnessSelection {
    type Err = String;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        match raw {
            "basic" => Ok(Self::Kind(HarnessKind::Basic)),
            "rlm" => Ok(Self::Kind(HarnessKind::Rlm)),
            "typescript" => Ok(Self::Kind(HarnessKind::TypeScript)),
            "exo" => Ok(Self::Kind(HarnessKind::Exo)),
            "codex" => Ok(Self::TypeScriptPreset(TypeScriptHarnessPreset::Codex)),
            "pi" => Ok(Self::TypeScriptPreset(TypeScriptHarnessPreset::Pi)),
            "claude-code" => Ok(Self::TypeScriptPreset(TypeScriptHarnessPreset::ClaudeCode)),
            "cursor" | "cursor-sdk" => Ok(Self::TypeScriptPreset(TypeScriptHarnessPreset::Cursor)),
            value if looks_like_typescript_module_path(value) => {
                Ok(Self::TypeScriptModule(PathBuf::from(value)))
            }
            _ => Err(format!(
                "unknown harness `{raw}`; expected basic, rlm, typescript, exo, codex, claude-code, cursor, or a TypeScript module path"
            )),
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum SecretBackendArg {
    #[value(name = "apple-keychain")]
    AppleKeychain,
    File,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum SandboxProviderArg {
    Daytona,
    #[value(name = "e2b")]
    E2b,
    #[value(name = "sprites")]
    Sprites,
    Vercel,
    #[value(name = "aws-agentcore")]
    AwsAgentCore,
    #[value(name = "apple-container")]
    AppleContainer,
    Docker,
    Smolvm,
    Firecracker,
    #[value(name = "local-process")]
    LocalProcess,
}

impl From<SandboxProviderArg> for SandboxProvider {
    fn from(value: SandboxProviderArg) -> Self {
        match value {
            SandboxProviderArg::Daytona => Self::Daytona,
            SandboxProviderArg::E2b => Self::E2b,
            SandboxProviderArg::Sprites => Self::Sprites,
            SandboxProviderArg::Vercel => Self::Vercel,
            SandboxProviderArg::AwsAgentCore => Self::AwsAgentCore,
            SandboxProviderArg::AppleContainer => Self::AppleContainer,
            SandboxProviderArg::Docker => Self::Docker,
            SandboxProviderArg::Smolvm => Self::Smolvm,
            SandboxProviderArg::Firecracker => Self::Firecracker,
            SandboxProviderArg::LocalProcess => Self::LocalProcess,
        }
    }
}

fn read_config_file<T: serde::de::DeserializeOwned>(path: &std::path::Path) -> Result<T> {
    let contents =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase);
    match extension.as_deref() {
        Some("json") => serde_json::from_str(&contents).context("invalid JSON"),
        Some("yaml" | "yml") => serde_yaml_ng::from_str(&contents).context("invalid YAML"),
        Some("toml") => toml::from_str(&contents).context("invalid TOML"),
        _ => anyhow::bail!("configuration file must use .json, .yaml, .yml, or .toml"),
    }
    .with_context(|| format!("parsing {}", path.display()))
}

fn build_exo_config(cli: &Cli) -> Result<BasicExoHarnessConfig> {
    let secret_backend = match cli
        .runtime()
        .secret_backend
        .unwrap_or_else(default_secret_backend)
    {
        SecretBackendArg::AppleKeychain => SecretBackendChoice::AppleKeychain,
        SecretBackendArg::File => SecretBackendChoice::File {
            path: cli.runtime().master_key_path.clone(),
        },
    };
    #[cfg(feature = "firecracker")]
    let firecracker_spec = match command_firecracker_args(&cli.command) {
        Some(args) => args.backend_spec()?,
        None => {
            let matches = FirecrackerArgs::augment_args(clap::Command::new("exo"))
                .try_get_matches_from(["exo"])?;
            <FirecrackerArgs as clap::FromArgMatches>::from_arg_matches(&matches)?.backend_spec()?
        }
    };
    #[cfg(not(feature = "firecracker"))]
    let firecracker_spec = FirecrackerBackendSpec::default();
    let sandbox_backends = default_sandbox_backends(firecracker_spec);
    let sandbox_policy = cli
        .execution()
        .and_then(|args| args.egress_policy.as_ref())
        .map(|path| read_config_file(path))
        .transpose()?;
    Ok(BasicExoHarnessConfig {
        root: cli.runtime().root.join("exoharness"),
        secret_backend,
        sandbox_default: default_local_sandbox_provider(),
        sandbox_policy,
        sandbox_backends,
    })
}

#[cfg(feature = "firecracker")]
fn command_firecracker_args(command: &Commands) -> Option<&FirecrackerArgs> {
    match command {
        Commands::Serve { args, .. } => Some(&args.firecracker),
        _ => None,
    }
}

/// Default providers: the OS-local container backend, local processes, and
/// Daytona (offered even with no key set — credentials resolve lazily).
fn default_sandbox_backends(
    firecracker: FirecrackerBackendSpec,
) -> Vec<SandboxBackendRegistration> {
    vec![
        SandboxBackendRegistration::apple_container(),
        SandboxBackendRegistration::docker(),
        SandboxBackendRegistration::smolvm(),
        SandboxBackendRegistration::firecracker(firecracker),
        SandboxBackendRegistration::local_process(),
        SandboxBackendRegistration::daytona(DaytonaBackendSpec::default()),
        SandboxBackendRegistration::e2b(E2bBackendSpec::default()),
        SandboxBackendRegistration::sprites(SpritesBackendSpec::default()),
        SandboxBackendRegistration::vercel(VercelBackendSpec::with_conventional_secrets()),
        SandboxBackendRegistration::aws_agentcore(),
    ]
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
fn default_local_sandbox_provider() -> SandboxProvider {
    SandboxProvider::Smolvm
}

#[cfg(not(target_os = "macos"))]
fn default_local_sandbox_provider() -> SandboxProvider {
    SandboxProvider::Docker
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum SandboxScopeArg {
    Agent,
    Conversation,
}

impl From<SandboxScopeArg> for SandboxScope {
    fn from(value: SandboxScopeArg) -> Self {
        match value {
            SandboxScopeArg::Agent => SandboxScope::Agent,
            SandboxScopeArg::Conversation => SandboxScope::Conversation,
        }
    }
}

// Provider and FirecrackerBridge return from run before runtime options are accessed.
macro_rules! runtime_accessor {
    ($name:ident $(, $mutable:tt)?) => {
        fn $name(&$($mutable)? self) -> &$($mutable)? RuntimeArgs {
            match &$($mutable)? self.command {
                Commands::Environment { runtime, .. }
                | Commands::Vault { runtime, .. }
                | Commands::Serve { runtime, .. }
                | Commands::Agent { runtime, .. }
                | Commands::Conversation { runtime, .. }
                => runtime,
                Commands::Provider { .. } | Commands::FirecrackerBridge => {
                    unreachable!("command does not use runtime options")
                }
            }
        }
    };
}

impl Cli {
    runtime_accessor!(runtime);
    runtime_accessor!(runtime_mut, mut);

    fn execution(&self) -> Option<&ExecutionArgs> {
        match &self.command {
            Commands::Agent {
                command: AgentCommands::Run { execution, .. },
                ..
            }
            | Commands::Conversation {
                command: ConversationCommands::Send { execution, .. },
                ..
            } => Some(execution),
            Commands::Serve { args, .. } => Some(&args.execution),
            _ => None,
        }
    }
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Configure reusable agent environments and their backends.
    Environment {
        #[command(flatten)]
        runtime: RuntimeArgs,
        #[command(subcommand)]
        command: environment::EnvironmentCommands,
    },
    /// Manage vaults and their credentials, including MCP login.
    Vault {
        #[command(flatten)]
        runtime: RuntimeArgs,
        #[command(subcommand)]
        command: vaults::VaultCommands,
    },
    #[command(hide = true)]
    FirecrackerBridge,
    /// Serve agents and vaults over HTTP.
    Serve {
        #[command(flatten)]
        runtime: RuntimeArgs,
        #[command(flatten)]
        args: Box<serve::ServeArgs>,
    },
    /// Create and run agents from Markdown specs.
    Agent {
        #[command(flatten)]
        runtime: RuntimeArgs,
        #[command(subcommand)]
        command: AgentCommands,
    },
    /// Manage saved threads and their history.
    #[command(name = "thread", alias = "conversation")]
    Conversation {
        #[command(flatten)]
        runtime: RuntimeArgs,
        #[command(subcommand)]
        command: ConversationCommands,
    },
    /// Connect to local or remote Exo providers and log in.
    Provider {
        #[command(subcommand)]
        command: Option<providers::ProviderCommands>,
    },
}

#[derive(Debug, Subcommand)]
enum AgentCommands {
    List,
    /// Save an agent from a Markdown spec.
    Create {
        name: String,
        #[arg(long)]
        file: PathBuf,
        #[arg(long)]
        slug: Option<String>,
    },
    /// Replace a saved agent's spec.
    Update {
        agent: String,
        #[arg(long)]
        file: PathBuf,
    },
    /// Run interactively, or execute one prompt with --prompt.
    Run {
        #[command(flatten)]
        execution: ExecutionArgs,
        #[command(flatten)]
        thread: Box<managed_agents::ThreadArgs>,
        #[arg(long, conflicts_with = "tui")]
        prompt: Option<String>,
        /// Use the full-screen TUI.
        #[arg(long)]
        tui: bool,
    },
    Mount {
        #[command(subcommand)]
        command: AgentMountCommands,
    },
    #[command(alias = "show")]
    Get {
        agent: String,
    },
    Delete {
        agent: String,
    },
}

#[derive(Debug, Subcommand)]
enum AgentMountCommands {
    List {
        agent: String,
    },
    #[command(alias = "add")]
    Create {
        agent: String,
        host_path: PathBuf,
        mount_path: Option<String>,
        #[arg(long)]
        rw: bool,
        #[arg(long)]
        internal: bool,
    },
    #[command(alias = "remove")]
    Delete {
        agent: String,
        mount_path: String,
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
        #[arg(long, value_enum)]
        sandbox_scope: Option<SandboxScopeArg>,
        #[command(flatten)]
        sandbox_runtime: ConversationSandboxRuntimeArgs,
    },
    Fork {
        agent: String,
        #[arg(value_name = "THREAD")]
        conversation: String,
        name: Option<String>,
        #[arg(long)]
        slug: Option<String>,
        #[arg(long)]
        up_to: Option<String>,
    },
    Update {
        agent: String,
        #[arg(value_name = "THREAD")]
        conversation: String,
        #[command(flatten)]
        sandbox_runtime: ConversationSandboxRuntimeUpdateArgs,
        #[arg(long, value_enum)]
        sandbox_scope: Option<SandboxScopeArg>,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        max_output_tokens: Option<i64>,
        #[arg(long)]
        clear_max_output_tokens: bool,
        #[arg(long)]
        clear_model_override: bool,
        /// Attach a vault without removing existing vaults or changing selected credentials.
        #[arg(long)]
        vault: Vec<String>,
    },
    Mount {
        #[command(subcommand)]
        command: ConversationMountCommands,
    },
    Sandbox {
        #[command(subcommand)]
        command: ConversationSandboxCommands,
    },
    #[command(alias = "show")]
    Get {
        agent: String,
        #[arg(value_name = "THREAD")]
        conversation: String,
    },
    Events {
        agent: String,
        #[arg(value_name = "THREAD")]
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
    /// Finalize a deferred rebuild_and_restart_exo outcome and record it in the
    /// conversation event log.
    CompleteRebuildUpdate {
        /// Path to the queued `.exo/guardian-updates/<id>.json` record.
        #[arg(long)]
        outcome: PathBuf,
        /// Final status: `succeeded` or `failed`.
        #[arg(long)]
        status: String,
        /// Guardian process exit code.
        #[arg(long)]
        exit_code: i32,
        /// Completion timestamp (UTC RFC3339).
        #[arg(long)]
        completed_at: String,
    },
    Send {
        #[command(flatten)]
        execution: ExecutionArgs,
        agent: String,
        #[arg(value_name = "THREAD")]
        conversation: String,
        prompt: String,
    },
    Delete {
        agent: String,
        #[arg(value_name = "THREAD")]
        conversation: String,
    },
}

#[derive(Debug, Subcommand)]
enum ConversationSandboxCommands {
    Attach {
        agent: String,
        #[arg(value_name = "THREAD")]
        conversation: String,
        #[arg(long = "sandbox", value_enum)]
        provider: SandboxProviderArg,
        #[arg(long)]
        external_id: String,
        #[arg(long)]
        default_workdir: Option<String>,
    },
    Detach {
        agent: String,
        #[arg(value_name = "THREAD")]
        conversation: String,
        sandbox_id: String,
    },
    Run {
        agent: String,
        #[arg(value_name = "THREAD")]
        conversation: String,
        command: String,
    },
}

#[derive(Debug, Clone, Default, Args)]
struct ConversationSandboxRuntimeArgs {
    #[arg(long)]
    sandbox_image: Option<String>,
    #[arg(long = "sandbox", value_enum)]
    sandbox_provider: Option<SandboxProviderArg>,
    #[arg(long)]
    shell_program: Option<String>,
}

impl ConversationSandboxRuntimeArgs {
    fn validate(&self) -> Result<()> {
        if self
            .sandbox_image
            .as_ref()
            .is_some_and(|image| image.trim().is_empty())
        {
            bail!("sandbox image must not be empty");
        }
        if self
            .shell_program
            .as_ref()
            .is_some_and(|program| program.trim().is_empty())
        {
            bail!("shell program must not be empty");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, Args)]
struct ConversationSandboxRuntimeUpdateArgs {
    #[command(flatten)]
    runtime: ConversationSandboxRuntimeArgs,
    #[arg(long)]
    clear_shell_program: bool,
    #[arg(long)]
    clear_sandbox_image: bool,
    #[arg(long = "clear-provider")]
    clear_sandbox_provider: bool,
}

impl ConversationSandboxRuntimeUpdateArgs {
    fn apply(self, config: &mut executor::ConversationConfig) -> Result<bool> {
        if self.clear_shell_program && self.runtime.shell_program.is_some() {
            bail!("provide either --clear-shell-program or --shell-program, not both");
        }
        if self.clear_sandbox_image && self.runtime.sandbox_image.is_some() {
            bail!("provide either --clear-sandbox-image or --sandbox-image, not both");
        }
        if self.clear_sandbox_provider && self.runtime.sandbox_provider.is_some() {
            bail!("provide either --clear-provider or --provider, not both");
        }
        self.runtime.validate()?;

        let mut changed = false;
        if self.clear_shell_program {
            config.shell_program = None;
            changed = true;
        } else if let Some(shell_program) = self.runtime.shell_program {
            config.shell_program = Some(shell_program);
            changed = true;
        }

        if self.clear_sandbox_image {
            config.sandbox_image = None;
            changed = true;
        } else if let Some(sandbox_image) = self.runtime.sandbox_image {
            config.sandbox_image = Some(sandbox_image);
            changed = true;
        }

        if self.clear_sandbox_provider {
            config.sandbox_provider = None;
            changed = true;
        } else if let Some(sandbox_provider) = self.runtime.sandbox_provider {
            config.sandbox_provider = Some(SandboxProvider::from(sandbox_provider));
            changed = true;
        }

        Ok(changed)
    }
}

#[derive(Debug, Subcommand)]
enum ConversationMountCommands {
    List {
        agent: String,
        #[arg(value_name = "THREAD")]
        conversation: String,
    },
    #[command(alias = "add")]
    Create {
        agent: String,
        #[arg(value_name = "THREAD")]
        conversation: String,
        host_path: PathBuf,
        mount_path: Option<String>,
        #[arg(long)]
        rw: bool,
        #[arg(long)]
        internal: bool,
    },
    #[command(alias = "remove")]
    Delete {
        agent: String,
        #[arg(value_name = "THREAD")]
        conversation: String,
        mount_path: String,
    },
}

struct CliError {
    error: anyhow::Error,
    verbose: bool,
}

impl std::fmt::Debug for CliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if !self.verbose
            && let Some(auth) = self.error.downcast_ref::<exo_mcp::McpAuthenticationError>()
        {
            let help = if auth.credential_supplied {
                "The server rejected the token. Check its validity and permissions."
            } else {
                "Add a secret for this MCP server URL to a selected vault, or attach the vault containing it, then start a new thread."
            };
            write!(f, "connecting MCP server {}. {help}", auth.server_name)
        } else if !self.verbose
            && let Some(context) = self.error.downcast_ref::<providers::ContextError>()
        {
            std::fmt::Display::fmt(context, f)
        } else {
            std::fmt::Debug::fmt(&self.error, f)
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), CliError> {
    let cli = Cli::parse();
    let verbose = matches!(&cli.command,
        Commands::Agent { command: AgentCommands::Run { thread, .. }, .. }
        if thread.verbosity == Verbosity::Full
    );
    if matches!(
        &cli.command,
        Commands::Agent {
            command: AgentCommands::Run { .. },
            ..
        }
    ) {
        turn_display::init_progress().map_err(|error| CliError { error, verbose })?;
    }
    run(cli).await.map_err(|error| CliError { error, verbose })
}

async fn run(mut cli: Cli) -> Result<()> {
    if matches!(cli.command, Commands::FirecrackerBridge) {
        #[cfg(feature = "firecracker")]
        {
            init_firecracker_bridge_tracing();
            if let Some(exit_code) = executor::run_firecracker_bridge().await? {
                std::process::exit(exit_code);
            }
            return Ok(());
        }
        #[cfg(not(feature = "firecracker"))]
        bail!("Firecracker bridge support requires building Exo with --features firecracker");
    }
    let config_directory = match cli.config_dir.clone() {
        Some(path) => path,
        None => cli
            .home
            .as_ref()
            .context("--config-dir or --home is required")?
            .join(".config/exo"),
    };
    let mut provider_store = providers::Store::load(config_directory)?;
    if let Commands::Provider { command } = &cli.command {
        return providers::run(command.as_ref(), &mut provider_store).await;
    }
    let serving = matches!(cli.command, Commands::Serve { .. });
    let (agent, thread) = command_refs_mut(&mut cli.command);
    let selected_provider = if serving && cli.provider_profile.is_none() {
        None
    } else {
        provider_store.selected(
            cli.provider_profile.as_deref(),
            agent.map(|v| v.as_str()),
            thread.map(|v| v.as_str()),
        )?
    };
    run_selected(cli, provider_store, selected_provider.as_ref())
        .await
        .map_err(|error| {
            if let Some(selection) = &selected_provider {
                providers::request_error(error, selection)
            } else {
                error
            }
        })
}

async fn run_selected(
    mut cli: Cli,
    mut provider_store: providers::Store,
    selected_provider: Option<&providers::Selection>,
) -> Result<()> {
    struct SelectedProvider<'a> {
        selection: &'a providers::Selection,
        account: String,
        client: Option<exo_managed_agents::http::RuntimeClient>,
    }
    let selected_provider = if let Some(selection) = selected_provider {
        let name = &selection.name;
        let (client, account) = match &provider_store.profile(name)?.connection {
            providers::Connection::Http { .. } => {
                let (client, account) = provider_store.client(selection).await?;
                (Some(client), account)
            }
            providers::Connection::Local { root } => {
                cli.runtime_mut().root = root.clone();
                (None, root.display().to_string())
            }
        };
        if !matches!(
            cli.command,
            Commands::Environment {
                command: environment::EnvironmentCommands::List
                    | environment::EnvironmentCommands::Get { .. }
                    | environment::EnvironmentCommands::Provider {
                        command: environment::ProviderCommands::List
                    },
                ..
            } | Commands::Agent {
                command: AgentCommands::List
                    | AgentCommands::Get { .. }
                    | AgentCommands::Mount {
                        command: AgentMountCommands::List { .. }
                    },
                ..
            } | Commands::Conversation {
                command: ConversationCommands::List { .. }
                    | ConversationCommands::Get { .. }
                    | ConversationCommands::Events { .. }
                    | ConversationCommands::Mount {
                        command: ConversationMountCommands::List { .. }
                    },
                ..
            } | Commands::Vault {
                command: vaults::VaultCommands::List { .. } | vaults::VaultCommands::Get { .. },
                ..
            }
        ) {
            eprintln!(
                "provider: {}; account: {}",
                name.escape_debug(),
                account.escape_debug()
            );
            if !selection.context.is_empty() {
                eprintln!("context: {}", serde_json::to_string(&selection.context)?);
            }
        }
        provider_store.resolve_aliases(&mut cli.command, &account)?;
        Some(SelectedProvider {
            selection,
            account,
            client,
        })
    } else {
        None
    };
    let http_client = selected_provider
        .as_ref()
        .and_then(|provider| provider.client.clone());
    let env = CliEnvironment::load(cli.runtime().env_file.as_deref())?;
    let definition = managed_agents::load_definition(&cli.command)?;

    let harness = providers::runtime(&cli, http_client, definition.as_ref(), &env).await?;
    let env_vars = env.into_vars();
    let root = cli.runtime().root.clone();
    let result: Result<()> = async {
    match cli.command {
        Commands::Environment { command, .. } => environment::run(harness.exoharness_handle().as_ref(), command).await?,
        Commands::FirecrackerBridge => {
            unreachable!("Firecracker bridge returns before harness startup")
        }
        Commands::Provider { .. } => {
            unreachable!("management commands return before harness startup")
        }
        Commands::Vault { command, .. } => vaults::run(harness.exoharness_handle().as_ref(), &command, &env_vars).await?,
        Commands::Agent { command: AgentCommands::Run { thread, tui, prompt, .. }, .. } => {
            let (agent, conversation) = managed_agents::open_thread(
                harness.as_ref(),
                definition.as_ref(),
                &thread,
            )
            .await?;
            if let Some(provider) = &selected_provider {
                provider_store.pin_thread(
                    conversation.record().slug.clone(),
                    provider.selection,
                    &provider.account,
                    agent.record().id,
                    conversation.record().id,
                )?;
            }
            let interactive = io::stdin().is_terminal() && io::stdout().is_terminal();
            if let Some(prompt) = prompt {
                tui::run_prompt(
                    Arc::clone(&harness), agent, conversation, thread.verbosity, &prompt,
                ).await?;
            } else if tui && interactive {
                tui_app::run_chat_tui(
                    Arc::clone(&harness),
                    agent,
                    conversation,
                    thread.verbosity,
                )
                .await?;
            } else {
                run_chat_repl(Arc::clone(&harness), agent, conversation, thread.verbosity).await?;
            }
        }
        Commands::Serve { args, .. } => {
            serve::run(harness.clone(), &root, *args).await?;
        }
        Commands::Agent { command, .. } => match command {
            AgentCommands::Run { .. } => unreachable!(),
            AgentCommands::List => {
                let agents = harness.list_agents().await?;
                print_table(
                    &["AGENT", "ID", "NAME"],
                    agents
                        .into_iter()
                        .filter(|agent| agent.slug != "__exo_sandbox_cli")
                        .map(|agent| vec![agent.slug, agent.id.to_string(), agent.name])
                        .collect(),
                )?;
            }
            AgentCommands::Create { name, file: _, slug } => {
                let slug = slug.unwrap_or_else(|| slugify(&name));
                anyhow::ensure!(!slug.is_empty(), "agent slug resolved to an empty value");
                let definition = definition.as_ref().context("agent spec is required")?;
                if selected_provider.is_some() {
                    provider_store.ensure_agent_alias_available(&slug)?;
                }
                eprintln!("Preparing agent resources...");
                let agent = harness.create_managed_agent(definition, &slug).await?;
                if let Some(provider) = &selected_provider {
                    provider_store.pin_agent(agent.record().slug.clone(), provider.selection, &provider.account, agent.record().id)?;
                }
                println!("created agent {} ({})", agent.record().slug, agent.record().id);
            }
            AgentCommands::Update { agent, file: _ } => {
                let agent = must_get_agent(harness.as_ref(), &agent).await?;
                eprintln!("Preparing agent resources...");
                harness.update_managed_agent(&agent, definition.as_ref().context("agent spec is required")?).await?;
                println!("updated agent {}", agent.record().slug);
            }
            AgentCommands::Mount { command } => match command {
                AgentMountCommands::List { agent } => {
                    let agent = must_get_agent(harness.as_ref(), &agent).await?;
                    let config = executor::load_agent_config(&*agent).await?;
                    print_mounts(&config.sandbox.mounts);
                }
                AgentMountCommands::Create {
                    agent,
                    host_path,
                    mount_path,
                    rw,
                    internal,
                } => {
                    let agent = must_get_agent(harness.as_ref(), &agent).await?;
                    let canonical_host_path = canonicalize_directory(&host_path)?;

                    let mut config = executor::load_agent_config(&*agent).await?;
                    let mount_path = match mount_path {
                        Some(mount_path) => {
                            validate_mount_path(&mount_path)?;
                            mount_path
                        }
                        None => default_mount_path(&canonical_host_path, &config.sandbox.mounts),
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
                        .sandbox
                        .mounts
                        .iter_mut()
                        .find(|mount| mount.mount_path == mount_path)
                    {
                        *existing = new_mount;
                    } else {
                        config.sandbox.mounts.push(new_mount);
                    }

                    harness.put_agent_config(&*agent, config).await?;
                    println!(
                        "mounted {} -> {} ({}) for agent {}",
                        canonical_host_path.display(),
                        mount_path,
                        if rw { "rw" } else { "ro" },
                        agent.record().slug
                    );
                }
                AgentMountCommands::Delete { agent, mount_path } => {
                    let agent = must_get_agent(harness.as_ref(), &agent).await?;
                    let mut config = executor::load_agent_config(&*agent).await?;
                    let before = config.sandbox.mounts.len();
                    config
                        .sandbox
                        .mounts
                        .retain(|mount| mount.mount_path != mount_path);
                    if config.sandbox.mounts.len() == before {
                        bail!("mount not found: {mount_path}");
                    }
                    harness.put_agent_config(&*agent, config).await?;
                    println!(
                        "removed mount {} from agent {}",
                        mount_path,
                        agent.record().slug
                    );
                }
            },
            AgentCommands::Get { agent } => {
                let agent = must_get_agent(harness.as_ref(), &agent).await?;
                println!("id: {}", agent.record().id);
                println!("slug: {}", agent.record().slug);
                println!("name: {}", agent.record().name);
                if let Some(definition) = exo_managed_agents::load_definition(agent.as_ref()).await? {
                    println!("{}", definition.source());
                }
                let Some(config) = executor::find_agent_config(agent.as_ref()).await? else {
                    return Ok(());
                };
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
                println!("typescript_tool_modules: {}", tool_module_paths.len());
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
                    config.sandbox.image.as_deref().unwrap_or("default")
                );
                println!(
                    "sandbox_provider: {}",
                    format_sandbox_provider(&config.sandbox.provider)
                );
                println!(
                    "sandbox_scope: {}",
                    match config.sandbox.scope {
                        executor::SandboxScope::Agent => "agent",
                        executor::SandboxScope::Conversation => "conversation",
                    }
                );
                println!("enable_networking: {}", config.sandbox.enable_networking);
                println!("sandbox_mounts:");
                print_mounts(&config.sandbox.mounts);
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
                let record = must_get_agent(harness.as_ref(), &agent).await?.record().clone();
                if !harness.delete_agent(&agent).await? {
                    bail!("agent not found: {agent}");
                }
                if let Some(provider) = &selected_provider {
                    provider_store.unpin_agent(&provider.selection.name, &provider.account, record.id)?;
                }
                println!("deleted agent {}", agent);
            }
        },
        Commands::Conversation { command, .. } => match command {
            ConversationCommands::List { agent } => {
                managed_agents::list_threads(harness.as_ref(), &agent).await?;
            }
            ConversationCommands::Create {
                agent,
                name,
                slug,
                sandbox_scope,
                sandbox_runtime,
            } => {
                sandbox_runtime.validate()?;
                let agent = must_get_agent(harness.as_ref(), &agent).await?;
                let slug = slug.unwrap_or_else(|| {
                    name.as_deref()
                        .map(slugify)
                        .filter(|slug| !slug.is_empty())
                        .unwrap_or_else(generate_fun_slug)
                });
                if slug.is_empty() {
                    bail!("conversation slug resolved to an empty value");
                }
                let conversation = harness
                    .create_conversation(
                        agent.as_ref(),
                        CreateConversationRequest {
                            vaults: vec![],
                            slug: Some(slug),
                            name,
                            sandbox_image: sandbox_runtime.sandbox_image,
                            sandbox_provider: sandbox_runtime
                                .sandbox_provider
                                .map(SandboxProvider::from),
                            shell_program: sandbox_runtime.shell_program,
                        },
                    )
                    .await?;
                if let Some(sandbox_scope) = sandbox_scope {
                    let mut config = executor::load_conversation_config(&*conversation).await?;
                    config.sandbox_scope = Some(sandbox_scope.into());
                    harness.put_conversation_config(&*conversation, config).await?;
                }
                println!(
                    "created conversation {} ({})",
                    conversation.record().slug,
                    conversation.record().id
                );
                println!(
                    "start chatting with it via `{}`",
                    chat_command(
                        agent.record().slug.as_str(),
                        conversation.record().slug.as_str(),
                    )
                );
            }
            ConversationCommands::Fork {
                agent,
                conversation,
                name,
                slug,
                up_to,
            } => {
                let source = must_get_conversation(harness.as_ref(), &agent, &conversation).await?;
                let forked = source
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
                println!(
                    "start chatting with it via `{}`",
                    chat_command(agent.as_str(), forked.record().slug.as_str())
                );
            }
            ConversationCommands::Update {
                agent,
                conversation,
                sandbox_scope,
                sandbox_runtime,
                model,
                max_output_tokens,
                clear_max_output_tokens,
                clear_model_override,
                vault,
            } => {
                if clear_model_override
                    && (model.is_some() || max_output_tokens.is_some() || clear_max_output_tokens)
                {
                    bail!(
                        "provide either --clear-model-override or model override flags, not both"
                    );
                }
                if clear_max_output_tokens && max_output_tokens.is_some() {
                    bail!(
                        "provide either --clear-max-output-tokens or --max-output-tokens, not both"
                    );
                }

                let agent_handle = must_get_agent(harness.as_ref(), &agent).await?;
                let conversation = harness
                    .get_conversation(agent_handle.as_ref(), &conversation)
                    .await?
                    .ok_or_else(|| anyhow!("conversation not found: {}", conversation))?;
                let mut config = executor::load_conversation_config(&*conversation).await?;
                let mut changed = sandbox_runtime.apply(&mut config)?;

                if let Some(sandbox_scope) = sandbox_scope {
                    config.sandbox_scope = Some(sandbox_scope.into());
                    changed = true;
                }

                let updated_model_override = if clear_model_override {
                    changed = true;
                    Some(None)
                } else if model.is_some() || max_output_tokens.is_some() || clear_max_output_tokens
                {
                    let agent_config = executor::load_agent_config(agent_handle.as_ref()).await?;
                    let mut model_override =
                        executor::get_conversation_model_override(&*conversation)
                            .await?
                            .unwrap_or(ConversationModelConfig {
                                model: agent_config.model,
                                max_output_tokens: agent_config.max_output_tokens,
                            });

                    if let Some(model) = model {
                        if model.trim().is_empty() {
                            bail!("model must not be empty");
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

                if !changed && vault.is_empty() {
                    bail!("no changes provided");
                }
                if !vault.is_empty() {
                    let root = harness.exoharness_handle();
                    let vaults = futures::future::try_join_all(
                        vault.iter().map(|name| {
                            exo_managed_agents::vaults::find_vault(root.as_ref(), name)
                        }),
                    )
                    .await?;
                    conversation
                        .attach_vaults(vaults.iter().map(|vault| vault.record().id).collect())
                        .await?;
                }

                if changed {
                    harness.put_conversation_config(&*conversation, config).await?;
                }
                if let Some(model_override) = updated_model_override {
                    executor::put_conversation_model_override(&*conversation, model_override).await?;
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
                    let config = executor::load_conversation_config(&*conversation).await?;
                    print_mounts(&config.mounts);
                }
                ConversationMountCommands::Create {
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

                    let mut config = executor::load_conversation_config(&*conversation).await?;
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

                    harness.put_conversation_config(&*conversation, config).await?;
                    println!(
                        "mounted {} -> {} ({}) for {}",
                        canonical_host_path.display(),
                        mount_path,
                        if rw { "rw" } else { "ro" },
                        conversation.record().slug
                    );
                }
                ConversationMountCommands::Delete {
                    agent,
                    conversation,
                    mount_path,
                } => {
                    let conversation =
                        must_get_conversation(harness.as_ref(), &agent, &conversation).await?;
                    let mut config = executor::load_conversation_config(&*conversation).await?;
                    let before = config.mounts.len();
                    config.mounts.retain(|mount| mount.mount_path != mount_path);
                    if config.mounts.len() == before {
                        bail!("mount not found: {mount_path}");
                    }
                    harness.put_conversation_config(&*conversation, config).await?;
                    println!(
                        "removed mount {} from {}",
                        mount_path,
                        conversation.record().slug
                    );
                }
            },
            ConversationCommands::Sandbox { command, .. } => match command {
                ConversationSandboxCommands::Attach {
                    agent,
                    conversation,
                    provider,
                    external_id,
                    default_workdir,
                } => {
                    let provider = SandboxProvider::from(provider);
                    let attachment = if provider == SandboxProvider::Docker {
                        SandboxAttachment::DockerContainer {
                            container_id: external_id,
                        }
                    } else {
                        bail!(
                            "sandbox provider {} does not support external attachments",
                            provider.as_str()
                        )
                    };
                    let conversation =
                        must_get_conversation(harness.as_ref(), &agent, &conversation).await?;
                    let sandbox_id = conversation
                        .attach_sandbox(AttachSandboxRequest {
                            attachment,
                            default_workdir,
                        })
                        .await?;
                    println!(
                        "attached Docker container as sandbox {} for {}",
                        sandbox_id,
                        conversation.record().slug
                    );
                }
                ConversationSandboxCommands::Detach {
                    agent,
                    conversation,
                    sandbox_id,
                } => {
                    let conversation =
                        must_get_conversation(harness.as_ref(), &agent, &conversation).await?;
                    let attachment = conversation
                        .detach_sandbox(sandbox_id.clone())
                        .await?;
                    println!(
                        "detached sandbox {} from {}: {}",
                        sandbox_id,
                        conversation.record().slug,
                        serde_json::to_string(&attachment)?
                    );
                }
                ConversationSandboxCommands::Run {
                    agent,
                    conversation,
                    command,
                } => {
                    let agent_handle = must_get_agent(harness.as_ref(), &agent).await?;
                    let conversation = harness
                        .get_conversation(agent_handle.as_ref(), &conversation)
                        .await?
                        .ok_or_else(|| anyhow!("conversation not found: {}", conversation))?;
                    let output = run_sandbox_shell_command(
                        agent_handle.as_ref(),
                        conversation.as_ref(),
                        command,
                    )
                    .await?;
                    io::stdout().write_all(output.stdout.as_bytes())?;
                    io::stderr().write_all(output.stderr.as_bytes())?;
                    if output.exit_code != 0 {
                        bail!("sandbox command exited with status {}", output.exit_code);
                    }
                }
            },
            ConversationCommands::Get {
                agent,
                conversation,
            } => {
                let agent_handle = must_get_agent(harness.as_ref(), &agent).await?;
                let conversation = harness
                    .get_conversation(agent_handle.as_ref(), &conversation)
                    .await?
                    .ok_or_else(|| anyhow!("conversation not found: {}", conversation))?;
                let messages =
                    executor::materialize_conversation_messages(conversation.as_ref()).await?;
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
                let Some(agent_config) = executor::find_agent_config(agent_handle.as_ref()).await? else {
                    render::print_transcript(&messages, Verbosity::default());
                    return Ok(());
                };
                let config = executor::load_conversation_config(&*conversation).await?;
                let model_override = executor::get_conversation_model_override(&*conversation).await?;
                let effective_model = model_override.clone().unwrap_or_else(|| ConversationModelConfig {
                    model: agent_config.model.clone(), max_output_tokens: agent_config.max_output_tokens,
                });
                println!(
                    "shell_program: {}",
                    config.shell_program.as_deref().unwrap_or("none")
                );
                println!(
                    "sandbox_scope: {}",
                    config
                        .sandbox_scope
                        .map(sandbox_scope_name)
                        .unwrap_or("default")
                );
                println!(
                    "effective_sandbox_scope: {}",
                    sandbox_scope_name(effective_sandbox_scope(&agent_config, &config))
                );
                println!(
                    "sandbox_image: {}",
                    config.sandbox_image.as_deref().unwrap_or("inherit")
                );
                println!(
                    "effective_sandbox_image: {}",
                    config
                        .effective_sandbox_image(&agent_config)
                        .unwrap_or("default")
                );
                println!(
                    "sandbox_provider: {}",
                    config
                        .sandbox_provider
                        .as_ref()
                        .map(format_sandbox_provider)
                        .unwrap_or("inherit")
                );
                println!(
                    "effective_sandbox_provider: {}",
                    format_sandbox_provider(&config.effective_sandbox_provider(&agent_config))
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
                        types: if types.is_empty() {
                            None
                        } else {
                            // User-supplied strings; `EventKind::custom` matches
                            // both known kinds (by name) and Custom events.
                            Some(types.into_iter().map(EventKind::custom).collect())
                        },
                    }))
                    .await?;
                println!("{}", serde_json::to_string_pretty(&result)?);
            }
            ConversationCommands::CompleteRebuildUpdate {
                outcome,
                status,
                exit_code,
                completed_at,
            } => {
                // Always finalize the side-channel outcome file first so a
                // conversation lookup or event-append failure cannot leave the
                // rebuild result only in process memory / logs.
                let completed =
                    finalize_rebuild_update_file(&outcome, &status, exit_code, &completed_at)?;
                let conversation = match must_get_conversation(
                    harness.as_ref(),
                    &completed.agent_id,
                    &completed.conversation_id,
                )
                .await
                {
                    Ok(conversation) => conversation,
                    Err(error) => match (
                        completed.agent_slug.as_deref(),
                        completed.conversation_slug.as_deref(),
                    ) {
                        (Some(agent), Some(conversation)) => {
                            must_get_conversation(harness.as_ref(), agent, conversation)
                                .await
                                .map_err(|slug_error| {
                                    error.context(format!(
                                        "also failed via slug {agent}/{conversation}: {slug_error}"
                                    ))
                                })?
                        }
                        _ => return Err(error),
                    },
                };
                record_host_event(
                    conversation.as_ref(),
                    HOST_EVENT_REBUILD_AND_RESTART,
                    serde_json::to_value(&completed)?,
                )
                .await?;
                println!("{}", serde_json::to_string_pretty(&completed)?);
            }
            ConversationCommands::Send {
                agent,
                conversation,
                prompt,
                ..
            } => {
                let conversation =
                    must_get_conversation(harness.as_ref(), &agent, &conversation).await?;
                let previous_messages =
                    executor::materialize_conversation_messages(conversation.as_ref()).await?;
                let agent = must_get_agent(harness.as_ref(), &agent).await?;
                send_conversation_wakeup(harness.as_ref(), &agent, &conversation, prompt).await?;
                let messages =
                    executor::materialize_conversation_messages(conversation.as_ref()).await?;
                for message in &messages[previous_messages.len()..] {
                    print_message(message, Verbosity::Full);
                }
            }
            ConversationCommands::Delete {
                agent,
                conversation,
            } => {
                let agent = must_get_agent(harness.as_ref(), &agent).await?;
                let thread = harness.get_conversation(agent.as_ref(), &conversation).await?;
                if !harness.delete_conversation(&*agent, &conversation).await? {
                    println!("conversation {} not found; nothing to delete", conversation);
                } else {
                    if let (Some(provider), Some(thread)) = (&selected_provider, thread) {
                        provider_store.unpin_thread(&provider.selection.name, &provider.account, thread.record().id)?;
                    }
                    println!("deleted conversation {}", conversation);
                }
            }
        },

    }

    Ok(())
    }.await;
    let shutdown = harness.shutdown().await;
    result?;
    shutdown
}

fn command_refs_mut(command: &mut Commands) -> (Option<&mut String>, Option<&mut String>) {
    match command {
        Commands::Agent { command, .. } => match command {
            AgentCommands::Update { agent, .. }
            | AgentCommands::Get { agent }
            | AgentCommands::Delete { agent } => (Some(agent), None),
            AgentCommands::Mount { command } => match command {
                AgentMountCommands::List { agent }
                | AgentMountCommands::Create { agent, .. }
                | AgentMountCommands::Delete { agent, .. } => (Some(agent), None),
            },
            AgentCommands::Run { thread, .. } => (thread.agent.as_mut(), thread.thread.as_mut()),
            AgentCommands::List | AgentCommands::Create { .. } => (None, None),
        },
        Commands::Conversation { command, .. } => match command {
            ConversationCommands::List { agent } | ConversationCommands::Create { agent, .. } => {
                (Some(agent), None)
            }
            ConversationCommands::Fork {
                agent,
                conversation,
                ..
            }
            | ConversationCommands::Update {
                agent,
                conversation,
                ..
            }
            | ConversationCommands::Get {
                agent,
                conversation,
            }
            | ConversationCommands::Events {
                agent,
                conversation,
                ..
            }
            | ConversationCommands::Send {
                agent,
                conversation,
                ..
            }
            | ConversationCommands::Delete {
                agent,
                conversation,
            } => (Some(agent), Some(conversation)),
            ConversationCommands::Mount { command } => match command {
                ConversationMountCommands::List {
                    agent,
                    conversation,
                }
                | ConversationMountCommands::Create {
                    agent,
                    conversation,
                    ..
                }
                | ConversationMountCommands::Delete {
                    agent,
                    conversation,
                    ..
                } => (Some(agent), Some(conversation)),
            },
            ConversationCommands::Sandbox { command, .. } => match command {
                ConversationSandboxCommands::Attach {
                    agent,
                    conversation,
                    ..
                }
                | ConversationSandboxCommands::Detach {
                    agent,
                    conversation,
                    ..
                }
                | ConversationSandboxCommands::Run {
                    agent,
                    conversation,
                    ..
                } => (Some(agent), Some(conversation)),
            },
            ConversationCommands::CompleteRebuildUpdate { .. } => (None, None),
        },
        Commands::FirecrackerBridge
        | Commands::Provider { .. }
        | Commands::Environment { .. }
        | Commands::Vault { .. } => (None, None),
        Commands::Serve { args, .. } => (args.agent.as_mut(), None),
    }
}

#[cfg(feature = "firecracker")]
fn init_firecracker_bridge_tracing() {
    let layer = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_target(false)
        .with_writer(std::io::stderr)
        .with_filter(tracing_subscriber::filter::LevelFilter::INFO);
    match tracing_subscriber::registry().with(layer).try_init() {
        Ok(()) | Err(_) => {}
    }
}

fn to_agent_harness_kind(kind: HarnessKind) -> AgentHarnessKind {
    match kind {
        HarnessKind::Basic => AgentHarnessKind::Basic,
        HarnessKind::Rlm => AgentHarnessKind::Rlm,
        HarnessKind::TypeScript => AgentHarnessKind::TypeScript,
        HarnessKind::Exo => AgentHarnessKind::Exo,
    }
}

fn format_harness_kind(kind: AgentHarnessKind) -> &'static str {
    match kind {
        AgentHarnessKind::Basic => "basic",
        AgentHarnessKind::Rlm => "rlm",
        AgentHarnessKind::TypeScript => "typescript",
        AgentHarnessKind::Exo => "exo",
    }
}

fn format_sandbox_provider(provider: &SandboxProvider) -> &str {
    provider.as_str()
}

async fn ensure_agent_matches_harness_selection(
    agent: &dyn AgentHandle,
    selection: &HarnessSelection,
) -> Result<()> {
    let config = executor::load_agent_config(agent).await?;
    let expected = to_agent_harness_kind(selection.harness_kind());
    if config.harness != expected {
        bail!(
            "agent {} is configured for {}; --harness {} requires {}",
            agent.record().slug,
            format_harness_kind(config.harness),
            format_harness_selection(selection),
            format_harness_kind(expected)
        );
    }

    if matches!(
        selection.harness_kind(),
        HarnessKind::TypeScript | HarnessKind::Exo
    ) && config.typescript.is_none()
    {
        bail!(
            "agent {} is configured for {} but has no module path",
            agent.record().slug,
            format_harness_selection(selection)
        );
    }

    let module = match selection {
        HarnessSelection::TypeScriptPreset(preset) => Some(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../..")
                .join(preset.module_path()),
        ),
        HarnessSelection::TypeScriptModule(module) => Some(module.clone()),
        HarnessSelection::Kind(_) => None,
    };
    if let Some(module) = module {
        let expected = module.canonicalize()?.to_string_lossy().into_owned();
        let actual = config
            .typescript
            .as_ref()
            .context("agent has no TypeScript module")?;
        anyhow::ensure!(
            actual.module_path == expected,
            "agent {} uses TypeScript module {}; --harness {} resolved to {}",
            agent.record().slug,
            actual.module_path,
            format_harness_selection(selection),
            expected
        );
    }

    Ok(())
}

fn looks_like_typescript_module_path(value: &str) -> bool {
    let path = Path::new(value);
    value.contains(std::path::MAIN_SEPARATOR)
        || path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| matches!(extension, "ts" | "tsx" | "js" | "mjs" | "cjs"))
}

fn format_harness_selection(selection: &HarnessSelection) -> String {
    match selection {
        HarnessSelection::Kind(kind) => match kind {
            HarnessKind::Basic => "basic".to_string(),
            HarnessKind::Rlm => "rlm".to_string(),
            HarnessKind::TypeScript => "typescript".to_string(),
            HarnessKind::Exo => "exo".to_string(),
        },
        HarnessSelection::TypeScriptPreset(preset) => match preset {
            TypeScriptHarnessPreset::Pi => "pi".to_string(),
            TypeScriptHarnessPreset::Codex => "codex".to_string(),
            TypeScriptHarnessPreset::ClaudeCode => "claude-code".to_string(),
            TypeScriptHarnessPreset::Cursor => "cursor".to_string(),
        },
        HarnessSelection::TypeScriptModule(path) => path.display().to_string(),
    }
}

fn print_table(headers: &[&str], rows: Vec<Vec<String>>) -> Result<()> {
    let stdout = io::stdout();
    let mut writer = TabWriter::new(stdout.lock()).padding(2);
    write_table_row(&mut writer, headers)?;
    for row in rows {
        write_table_row(&mut writer, &row)?;
    }
    writer.flush()?;
    Ok(())
}

fn write_table_row<T: AsRef<str>, W: Write>(writer: &mut W, values: &[T]) -> io::Result<()> {
    for (index, value) in values.iter().enumerate() {
        if index > 0 {
            write!(writer, "\t")?;
        }
        write!(writer, "{}", value.as_ref())?;
    }
    writeln!(writer)
}

async fn find_secret_id(
    exoharness: &dyn ExoHarness,
    vault: &str,
    name: &str,
) -> Result<Option<SecretReference>> {
    let vault = exo_managed_agents::vaults::find_vault(exoharness, vault).await?;
    Ok(vault
        .list_secrets()
        .await?
        .into_iter()
        .rev()
        .find(|secret| secret.name == name)
        .map(|secret| SecretReference {
            vault_id: vault.record().id,
            secret_id: secret.id,
        }))
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

fn parse_optional_uuid7(value: Option<&str>, field: &str) -> Result<Option<Uuid7>> {
    match value {
        Some(value) => Ok(Some(
            value
                .parse::<Uuid7>()
                .map_err(|error| anyhow!("invalid {field}: {error}"))?,
        )),
        None => Ok(None),
    }
}

fn parse_sandbox_mount(value: &str) -> std::result::Result<FileSystemMount, String> {
    let (host_path, mount_path, mode) = parse_mount_spec(value, "host path")?;
    Ok(FileSystemMount {
        host_path: host_path.to_string(),
        mount_path: mount_path.to_string(),
        mode,
        internal: Some(false),
    })
}

fn parse_mount_spec<'a>(
    value: &'a str,
    source_label: &str,
) -> std::result::Result<(&'a str, &'a str, FileSystemMountMode), String> {
    let (source, target) = value
        .split_once(':')
        .ok_or_else(|| format!("expected {source_label}:GUEST_PATH[:ro|rw]"))?;
    if source.is_empty() {
        return Err(format!("{source_label} must not be empty"));
    }
    let (mount_path, mode) = match target.rsplit_once(':') {
        Some((mount_path, "ro")) => (mount_path, FileSystemMountMode::ReadOnly),
        Some((mount_path, "rw")) => (mount_path, FileSystemMountMode::ReadWrite),
        _ => (target, FileSystemMountMode::ReadOnly),
    };
    validate_mount_path(mount_path).map_err(|error| error.to_string())?;
    Ok((source, mount_path, mode))
}

fn canonicalize_directory(path: &Path) -> Result<PathBuf> {
    let canonical = std::fs::canonicalize(path)?;
    if !canonical.is_dir() {
        bail!(
            "mount host path is not a directory: {}",
            canonical.display()
        );
    }
    Ok(canonical)
}

fn validate_mount_path(mount_path: &str) -> Result<()> {
    if mount_path.trim().is_empty() {
        bail!("mount path must not be empty");
    }
    if !mount_path.starts_with('/') {
        bail!("mount path must be absolute");
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

async fn must_get_agent(harness: &Runtime, agent_ref: &str) -> Result<Arc<dyn AgentHandle>> {
    harness
        .get_agent(agent_ref)
        .await?
        .ok_or_else(|| anyhow!("agent not found: {agent_ref}"))
}

async fn must_get_conversation(
    harness: &Runtime,
    agent_ref: &str,
    conversation_ref: &str,
) -> Result<Arc<dyn ConversationHandle>> {
    let agent = must_get_agent(harness, agent_ref).await?;
    harness
        .get_conversation(&*agent, conversation_ref)
        .await?
        .ok_or_else(|| anyhow!("conversation not found: {conversation_ref}"))
}

#[derive(Debug, Deserialize)]
struct SandboxShellOutput {
    stdout: String,
    stderr: String,
    exit_code: i32,
}

async fn run_sandbox_shell_command(
    agent: &dyn AgentHandle,
    conversation: &dyn ConversationHandle,
    command: String,
) -> Result<SandboxShellOutput> {
    let agent_config = executor::load_agent_config(agent).await?;
    let mut config = executor::load_conversation_config(conversation).await?;
    if config.shell_program.is_none() {
        bail!(
            "shell sandbox is not enabled for this conversation; run `exo thread update {} {} --shell-program /bin/bash`",
            agent.record().slug,
            conversation.record().slug
        );
    }
    config
        .materialize_resources(conversation, &agent_config)
        .await?;
    tracing::info!(target: "exoharness::progress", "Running sandbox command");
    let runtime = BasicToolRuntime;

    let mut arguments = serde_json::Map::new();
    arguments.insert("command".to_string(), serde_json::Value::String(command));
    let result = runtime
        .execute(
            agent,
            conversation,
            None,
            &agent_config,
            &config,
            &ToolRequest {
                namespace: None,
                function_name: "shell".to_string(),
                arguments,
            },
        )
        .await?;
    Ok(serde_json::from_value(result)?)
}

fn chat_command(agent_slug: &str, conversation_slug: &str) -> String {
    format!("exo agent run --agent {agent_slug} --thread {conversation_slug}")
}

fn sandbox_scope_name(scope: SandboxScope) -> &'static str {
    match scope {
        SandboxScope::Agent => "agent",
        SandboxScope::Conversation => "conversation",
    }
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

fn parse_env_var_name(value: &str) -> std::result::Result<String, String> {
    if is_env_var_name(value) {
        Ok(value.to_string())
    } else {
        Err(
            "pass an environment variable name such as OPENAI_API_KEY, not the secret value"
                .to_string(),
        )
    }
}

fn env_value_from_arg(
    flag: &str,
    env: &str,
    loaded_env: &HashMap<String, String>,
) -> Result<String> {
    if !is_env_var_name(env) {
        bail!(
            "invalid {flag} value; pass an environment variable name such as OPENAI_API_KEY, not the secret value"
        );
    }

    loaded_env
        .get(env)
        .cloned()
        .or_else(|| std::env::var(env).ok())
        .ok_or_else(|| anyhow!("environment variable passed to {flag} is not set"))
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
    let word_a = SLUG_WORDS_A[(bytes[10] as usize) % SLUG_WORDS_A.len()];
    let word_b = SLUG_WORDS_B[(bytes[11] as usize) % SLUG_WORDS_B.len()];
    let suffix = format!(
        "{:02x}{:02x}{:02x}{:02x}",
        bytes[12], bytes[13], bytes[14], bytes[15]
    );
    format!("{word_a}-{word_b}-{suffix}")
}

#[cfg(test)]
mod command_tests {
    use super::*;

    #[test]
    fn command_tree_is_consistent() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
        for command in [
            "chat",
            "run",
            "secret",
            "model",
            "sandbox-provider",
            "sandbox",
            "environments",
            "tools",
            "adapters",
        ] {
            assert!(Cli::try_parse_from(["exo", command]).is_err());
        }
        for args in [
            vec!["agent", "create", "support", "--file", "agent.md"],
            vec!["agent", "update", "support", "--file", "agent.md"],
            vec!["serve", "--agent", "support"],
            vec!["environment", "provider", "list"],
        ] {
            Cli::try_parse_from(["exo"].into_iter().chain(args)).unwrap();
        }
        for args in [
            vec!["agent", "serve", "support"],
            vec!["agent", "create", "support", "--model", "test"],
            vec!["agent", "--exoharness-url", "http://localhost", "list"],
            vec![
                "agent", "run", "--agent", "support", "--tui", "--prompt", "hi",
            ],
        ] {
            assert!(Cli::try_parse_from(["exo"].into_iter().chain(args)).is_err());
        }
    }

    #[test]
    fn execution_options_only_belong_to_execution_commands() {
        for command in [
            vec![
                "vault",
                "secret",
                "create",
                "team",
                "git",
                "--token-env",
                "TOKEN",
            ],
            vec!["environment", "list"],
            vec!["environment", "provider", "list"],
            vec!["agent", "get", "test"],
            vec!["thread", "list", "test"],
        ] {
            let help = Cli::try_parse_from(
                ["exo"]
                    .into_iter()
                    .chain(command.iter().copied())
                    .chain(["--help"]),
            )
            .unwrap_err()
            .to_string();
            for option in [
                "--pricing-path",
                "--pricing-url",
                "--egress-policy",
                "--harness",
                "--env-file-if-exists",
            ] {
                assert!(!help.contains(option), "{help}");
                let error = Cli::try_parse_from(
                    ["exo"]
                        .into_iter()
                        .chain(command.iter().copied())
                        .chain([option, "unused"]),
                )
                .unwrap_err();
                assert_eq!(
                    error.kind(),
                    clap::error::ErrorKind::UnknownArgument,
                    "{error}"
                );
            }
            assert!(help.contains("--env-file"), "{help}");
        }
        for command in [
            vec!["agent", "run", "--agent", "test"],
            vec!["thread", "send", "test", "thread", "hi"],
            vec!["serve"],
        ] {
            let cli = Cli::try_parse_from(["exo"].into_iter().chain(command).chain([
                "--pricing-path",
                "prices.json",
                "--pricing-url",
                "https://example.com/prices.json",
                "--egress-policy",
                "policy.json",
                "--harness",
                "codex",
            ]))
            .unwrap();
            assert!(cli.execution().unwrap().pricing_path.is_some());
        }
    }

    #[test]
    fn interactive_and_one_shot_runs_share_thread_selection() {
        for prompt in [None, Some("hello")] {
            let mut args = vec![
                "exo",
                "agent",
                "run",
                "--agent",
                "support",
                "--thread",
                "saved",
                "--harness",
                "codex",
            ];
            if let Some(prompt) = prompt {
                args.extend(["--prompt", prompt]);
            }
            let cli = Cli::try_parse_from(args).unwrap();
            assert!(matches!(cli.command,
                Commands::Agent { command: AgentCommands::Run { thread, prompt: actual, .. }, .. }
                if thread.agent.as_deref() == Some("support") && thread.thread.as_deref() == Some("saved")
                    && actual.as_deref() == prompt
            ));
        }
        assert_eq!(
            chat_command("support", "saved"),
            "exo agent run --agent support --thread saved"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_defaults_to_smolvm() {
        assert_eq!(
            super::default_local_sandbox_provider(),
            super::SandboxProvider::Smolvm
        );
    }
}
