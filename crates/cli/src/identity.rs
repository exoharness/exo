use anyhow::{Context, Result};
use clap::Subcommand;
use executor::Runtime;
use exo_managed_agents::{http::RuntimeClient, identity::IdentityCommand};
use std::{path::Path, sync::Arc};

#[derive(Debug, Subcommand)]
pub enum IdentityCommands {
    /// List user vault associations and trusted Slack workspaces.
    List,
    /// Pin a Slack sender to an existing Exo account or email.
    Bind {
        #[command(subcommand)]
        source: IdentitySource,
    },
    /// Remove a Slack sender pin.
    Unbind {
        #[command(subcommand)]
        source: SlackSender,
    },
    /// Trust a Slack workspace's directory for email matching.
    Trust {
        #[command(subcommand)]
        source: SlackWorkspace,
    },
    /// Stop trusting a Slack workspace's directory.
    Untrust {
        #[command(subcommand)]
        source: SlackWorkspace,
    },
}
#[derive(Debug, Subcommand)]
pub enum IdentitySource {
    Slack {
        #[arg(long)]
        workspace: String,
        #[arg(long)]
        user: String,
        #[arg(long)]
        account: String,
    },
}
#[derive(Debug, Subcommand)]
pub enum SlackSender {
    Slack {
        #[arg(long)]
        workspace: String,
        #[arg(long)]
        user: String,
    },
}
#[derive(Debug, Subcommand)]
pub enum SlackWorkspace {
    Slack {
        #[arg(long)]
        workspace: String,
    },
}
impl IdentityCommands {
    pub fn into_request(self) -> IdentityCommand {
        match self {
            Self::List => IdentityCommand::List,
            Self::Bind {
                source:
                    IdentitySource::Slack {
                        workspace,
                        user,
                        account,
                    },
            } => IdentityCommand::BindSlack {
                workspace,
                user,
                account,
            },
            Self::Unbind {
                source: SlackSender::Slack { workspace, user },
            } => IdentityCommand::UnbindSlack { workspace, user },
            Self::Trust {
                source: SlackWorkspace::Slack { workspace },
            } => IdentityCommand::TrustSlack {
                workspace,
                trusted: true,
            },
            Self::Untrust {
                source: SlackWorkspace::Slack { workspace },
            } => IdentityCommand::TrustSlack {
                workspace,
                trusted: false,
            },
        }
    }
}
pub async fn run(
    runtime: &Arc<Runtime>,
    client: Option<&RuntimeClient>,
    root: Option<&Path>,
    command: IdentityCommand,
) -> Result<()> {
    let configuration = if let Some(client) = client {
        client.manage_identity(&command).await?
    } else {
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(
                root.context("local identity requires state root")?
                    .join("service.lock"),
            )?;
        lock.try_lock().context("the local server is running; use an HTTP provider to manage its identities, or stop it first")?;
        executor::remote::IdentityRegistry::open(runtime.exoharness_handle())
            .await?
            .execute(command)
            .await?
    };
    println!("{}", serde_json::to_string_pretty(&configuration)?);
    Ok(())
}
