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
    /// Log in and save a credential in a vault.
    Login(login::LoginArgs),
    /// Import, update, or delete credentials in a vault.
    Secret {
        #[command(subcommand)]
        command: SecretCommands,
    },
}

#[derive(Debug, Args, Default)]
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
    /// Import a token from an environment variable.
    Create {
        vault: String,
        name: String,
        #[command(flatten)]
        policy: PolicyArgs,
        /// Read the secret value from this environment variable.
        #[arg(long, value_parser = crate::parse_env_var_name)]
        token_env: String,
    },
    /// Rotate a token or replace its policy; omitted fields are preserved.
    Update {
        vault: String,
        secret: String,
        #[command(flatten)]
        policy: PolicyArgs,
        /// Read the new secret value from this environment variable.
        #[arg(long, value_parser = crate::parse_env_var_name)]
        token_env: Option<String>,
    },
    /// Delete a secret from the vault.
    Delete { vault: String, secret: String },
}

pub async fn run(
    store: &dyn ExoHarness,
    command: &VaultCommands,
    env: &HashMap<String, String>,
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
                    "No secrets. Use `exo vault login {}` or `exo vault secret create {}`.",
                    vault.record().name,
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
                                    exoharness::SecretType::Key => "token",
                                    exoharness::SecretType::Oauth => "oauth",
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
        VaultCommands::Login(args) => login::run(store, args, env).await?,
        VaultCommands::Secret { command } => {
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
                    token_env,
                    ..
                } => {
                    let id = vault
                        .put_secret(PutSecretRequest {
                            name: name.clone(),
                            policy: policy.resolve()?,
                            secret: token(token_env, env)?,
                        })
                        .await?;
                    println!("created secret {name} ({id})");
                }
                SecretCommands::Update {
                    secret,
                    policy,
                    token_env,
                    ..
                } => {
                    let record = find_secret(vault.list_secrets().await?, secret)?;
                    let policy = policy.resolve()?;
                    let secret = token_env
                        .as_deref()
                        .map(|variable| token(variable, env))
                        .transpose()?;
                    ensure!(
                        secret.is_some() || policy.is_some(),
                        "provide --token-env or a policy to update; use `exo vault login --replace` to log in again"
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

fn token(variable: &str, env: &HashMap<String, String>) -> Result<Secret> {
    Ok(Secret::Key {
        value: crate::env_value_from_arg("--token-env", variable, env)?,
    })
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
