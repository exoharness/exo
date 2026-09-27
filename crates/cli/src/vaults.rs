mod login;

use anyhow::{Context, Result, bail, ensure};
use clap::{Args, Subcommand};
use exo_managed_agents::vaults::find_vault;
use exoharness::{
    CredentialDestination, CredentialPolicy, ExoHarness, PutSecretRequest, Secret, SecretMetadata,
};
use std::{collections::HashMap, path::PathBuf};

#[derive(Debug, Subcommand)]
pub enum VaultCommands {
    /// Create a vault.
    Create { name: String },
    /// List vaults, or the secrets inside a vault.
    List { vault: Option<String> },
    /// Show vault or secret metadata (never the credential value).
    Get {
        vault: String,
        secret: Option<String>,
    },
    /// Delete a vault and its secrets.
    Delete { vault: String },
    /// Import, update, or delete credentials in a vault.
    Secret {
        #[command(subcommand)]
        command: Box<SecretCommands>,
    },
}

#[derive(Debug, Args, Default)]
#[command(next_help_heading = "Credential permissions")]
pub(super) struct PolicyArgs {
    /// Permit credential use on this HTTPS origin (HTTP allowed on loopback). Repeat for multiple origins.
    #[arg(long)]
    allow_origin: Vec<String>,
    /// Permit credential use at this exact HTTP(S) resource URL. Repeat as needed.
    #[arg(long)]
    allow_url: Vec<String>,
    /// Credential policy file (JSON, YAML, or TOML).
    #[arg(long, conflicts_with_all = ["allow_origin", "allow_url"])]
    policy: Option<PathBuf>,
}

impl PolicyArgs {
    fn resolve(&self) -> Result<Option<CredentialPolicy>> {
        if let Some(path) = &self.policy {
            let policy: CredentialPolicy = crate::read_config_file(path)?;
            return Ok(Some(policy.normalized()?));
        }
        let destinations = self
            .allow_origin
            .iter()
            .map(|value| CredentialDestination::origin(value))
            .chain(
                self.allow_url
                    .iter()
                    .map(|value| CredentialDestination::url(value)),
            )
            .collect::<Result<Vec<_>>>()?;
        Ok((!destinations.is_empty()).then(|| CredentialPolicy::destinations(destinations)))
    }
}

#[derive(Debug, Subcommand)]
pub enum SecretCommands {
    /// Create a secret by importing a token or logging in.
    #[command(group(clap::ArgGroup::new("source").required(true).multiple(true)
        .args(["token_env", "preset", "url", "client_id"])))]
    Create {
        vault: String,
        /// Secret name; defaults to the preset name.
        #[arg(required_unless_present = "preset")]
        name: Option<String>,
        #[command(flatten)]
        credential: login::CredentialArgs,
        #[command(flatten)]
        policy: PolicyArgs,
    },
    /// Rotate a token, log in again, or change permissions; omitted fields are preserved.
    Update {
        vault: String,
        secret: String,
        #[command(flatten)]
        credential: login::CredentialArgs,
        #[command(flatten)]
        policy: PolicyArgs,
    },
    /// Delete a secret from the vault.
    Delete { vault: String, secret: String },
}

pub async fn run(
    store: &dyn ExoHarness,
    command: &VaultCommands,
    env: &HashMap<String, String>,
    local: bool,
) -> Result<()> {
    match command {
        VaultCommands::Create { name } => {
            let vault = store.create_vault(name).await?;
            println!(
                "created vault {} ({})",
                vault.record().name,
                vault.record().id
            );
        }
        VaultCommands::List { vault: None } => {
            let mut rows = Vec::new();
            for vault in store.list_vaults().await? {
                rows.push(vec![
                    vault.record().name.clone(),
                    vault.list_secrets().await?.len().to_string(),
                    vault.record().id.to_string(),
                ]);
            }
            crate::print_table(&["VAULT", "SECRETS", "ID"], rows)?;
            println!("Use `exo vault list <vault>` to see its secrets.");
        }
        VaultCommands::List { vault: Some(vault) } => {
            let vault = find_vault(store, vault).await?;
            println!("Secrets in vault {}:", vault.record().name);
            let secrets = vault.list_secrets().await?;
            if secrets.is_empty() {
                println!(
                    "No secrets. Use `exo vault secret create {}`.",
                    vault.record().name
                );
            } else {
                crate::print_table(
                    &["SECRET", "TYPE", "DESTINATIONS", "ID"],
                    secrets
                        .into_iter()
                        .map(|secret| {
                            let destinations = secret
                                .policy
                                .map(|policy| match policy.networking {
                                    exoharness::CredentialNetworkPolicy::Limited {
                                        allowed_hosts,
                                    } => allowed_hosts.join(", "),
                                    exoharness::CredentialNetworkPolicy::Destinations {
                                        allowed_destinations,
                                    } => allowed_destinations
                                        .iter()
                                        .map(CredentialDestination::as_str)
                                        .collect::<Vec<_>>()
                                        .join(", "),
                                })
                                .unwrap_or_else(|| "none".into());
                            vec![
                                secret.name,
                                match secret.r#type {
                                    exoharness::SecretType::Key => "key",
                                    exoharness::SecretType::Oauth => "oauth",
                                    exoharness::SecretType::GithubCli => "github_cli",
                                }
                                .into(),
                                destinations,
                                secret.id.to_string(),
                            ]
                        })
                        .collect(),
                )?;
            }
        }
        VaultCommands::Get { vault, secret } => {
            let vault = find_vault(store, vault).await?;
            let json = if let Some(secret) = secret {
                serde_json::to_string_pretty(&find_secret(vault.list_secrets().await?, secret)?)?
            } else {
                serde_json::to_string_pretty(vault.record())?
            };
            println!("{json}");
        }
        VaultCommands::Delete { vault } => {
            let vault = find_vault(store, vault).await?;
            store.delete_vault(&vault.record().id).await?;
            println!(
                "deleted vault {} ({})",
                vault.record().name,
                vault.record().id
            );
        }
        VaultCommands::Secret { command } => {
            let command = command.as_ref();
            let vault = match command {
                SecretCommands::Create { vault, .. }
                | SecretCommands::Update { vault, .. }
                | SecretCommands::Delete { vault, .. } => vault,
            };
            let vault = find_vault(store, vault).await?;
            match command {
                SecretCommands::Create {
                    name,
                    policy,
                    credential,
                    ..
                } => {
                    let name = name
                        .as_deref()
                        .or(credential.default_name())
                        .context("provide a secret name")?;
                    ensure!(!name.trim().is_empty(), "secret name must not be empty");
                    ensure!(
                        !vault
                            .list_secrets()
                            .await?
                            .iter()
                            .any(|secret| secret.name == name),
                        "secret {name:?} already exists; use `exo vault secret update`"
                    );
                    let policy = policy.resolve()?.or(credential.default_policy()?);
                    let secret = credential
                        .read(env, policy.as_ref(), local)
                        .await?
                        .context(
                            "provide --token-env, --preset, --url, or OAuth device endpoints",
                        )?;
                    let id = vault
                        .put_secret(PutSecretRequest {
                            name: name.into(),
                            policy,
                            secret,
                        })
                        .await?;
                    println!("created secret {name} ({id})");
                }
                SecretCommands::Update {
                    secret,
                    policy,
                    credential,
                    ..
                } => {
                    let record = find_secret(vault.list_secrets().await?, secret)?;
                    let policy = policy.resolve()?.or(if record.policy.is_none() {
                        credential.default_policy()?
                    } else {
                        None
                    });
                    let secret = credential
                        .read(env, policy.as_ref().or(record.policy.as_ref()), local)
                        .await?;
                    ensure!(
                        secret.is_some() || policy.is_some(),
                        "provide --token-env to rotate, --preset or --url to log in again, or a policy to update"
                    );
                    let record = vault
                        .update_secret(
                            &record.id,
                            exoharness::UpdateSecretRequest { secret, policy },
                        )
                        .await?;
                    println!(
                        "updated secret {} ({}) to revision {}",
                        record.name, record.id, record.revision
                    );
                }
                SecretCommands::Delete { secret, .. } => {
                    let record = find_secret(vault.list_secrets().await?, secret)?;
                    vault.delete_secret(&record.id).await?;
                    println!("deleted secret {} ({})", record.name, record.id);
                }
            }
        }
    }
    Ok(())
}

fn find_secret(secrets: Vec<SecretMetadata>, reference: &str) -> Result<SecretMetadata> {
    let id = reference.parse::<exoharness::Uuid7>().ok();
    let mut matches = secrets
        .into_iter()
        .filter(|c| Some(c.id) == id || c.name == reference);
    let record = matches
        .next()
        .with_context(|| format!("secret not found: {reference}"))?;
    if matches.next().is_some() {
        bail!("ambiguous secret reference: {reference}; use its id");
    }
    Ok(record)
}
