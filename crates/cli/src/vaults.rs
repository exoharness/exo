use oauth2::TokenResponse;
use rmcp::transport::auth::AuthorizationManager;
use std::{collections::HashMap, time::Duration};

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use exo_managed_agents::vaults::find_vault;
use exoharness::vault::SecretTarget;
use exoharness::{ExoHarness, PutSecretRequest, Secret, SecretMetadata};

#[derive(Debug, Subcommand)]
pub enum VaultCommands {
    Create {
        name: String,
    },
    List,
    Get {
        vault: String,
    },
    Delete {
        vault: String,
    },
    Secret {
        #[command(subcommand)]
        command: SecretCommands,
    },
}

#[derive(Debug, Subcommand)]
pub enum SecretCommands {
    Create {
        vault: String,
        name: String,
        #[arg(long, conflicts_with = "http_origin")]
        mcp_server_url: Option<String>,
        #[arg(long)]
        http_origin: Option<String>,
        #[arg(long, value_parser = crate::parse_env_var_name)]
        token_env: Option<String>,
        #[arg(long, conflicts_with = "token_env")]
        no_browser: bool,
    },
    List {
        vault: String,
    },
    Get {
        vault: String,
        secret: String,
    },
    Update {
        vault: String,
        secret: String,
        #[arg(long, value_parser = crate::parse_env_var_name)]
        token_env: Option<String>,
        #[arg(long, conflicts_with = "token_env")]
        no_browser: bool,
    },
    Delete {
        vault: String,
        secret: String,
    },
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
        VaultCommands::List => {
            crate::print_table(
                &["VAULT", "ID"],
                store
                    .list_vaults()
                    .await?
                    .into_iter()
                    .map(|v| vec![v.record().name.clone(), v.record().id.to_string()])
                    .collect(),
            )?;
        }
        VaultCommands::Get { vault } => {
            println!(
                "{}",
                serde_json::to_string_pretty(find_vault(store, vault).await?.record())?
            );
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
            let vault = match command {
                SecretCommands::Create { vault, .. }
                | SecretCommands::List { vault }
                | SecretCommands::Get { vault, .. }
                | SecretCommands::Update { vault, .. }
                | SecretCommands::Delete { vault, .. } => vault,
            };
            let vault = find_vault(store, vault).await?;
            match command {
                SecretCommands::Create {
                    name,
                    mcp_server_url,
                    http_origin,
                    token_env,
                    no_browser,
                    ..
                } => {
                    let target = mcp_server_url
                        .as_deref()
                        .map(SecretTarget::mcp)
                        .transpose()?
                        .or(http_origin.as_deref().map(SecretTarget::http).transpose()?);
                    if vault.list_secrets().await?.iter().any(|secret| {
                        secret.name == *name
                            || matches!(target, Some(SecretTarget::Mcp { .. }))
                                && secret.target == target
                    }) {
                        bail!(
                            "a secret with this name or MCP destination already exists; use exo vault secret update"
                        );
                    }
                    let secret =
                        credential(token_env.as_deref(), target.as_ref(), *no_browser, env).await?;
                    let secret_id = vault
                        .put_secret(PutSecretRequest {
                            name: name.clone(),
                            target,
                            secret,
                        })
                        .await?;
                    println!("created secret {name} ({secret_id})");
                }
                SecretCommands::List { .. } => {
                    crate::print_table(
                        &["SECRET", "ID", "TYPE", "DESTINATION", "REVISION"],
                        vault
                            .list_secrets()
                            .await?
                            .into_iter()
                            .map(|c| {
                                let server_url = match c.target {
                                    Some(SecretTarget::Mcp { server_url }) => server_url,
                                    Some(SecretTarget::Http { origin }) => origin,
                                    None => String::new(),
                                };
                                vec![
                                    c.name,
                                    c.id.to_string(),
                                    match c.r#type {
                                        exoharness::SecretType::Key => "key",
                                        exoharness::SecretType::Oauth => "oauth",
                                    }
                                    .into(),
                                    server_url,
                                    c.revision.to_string(),
                                ]
                            })
                            .collect(),
                    )?;
                }
                SecretCommands::Get { secret, .. }
                | SecretCommands::Update { secret, .. }
                | SecretCommands::Delete { secret, .. } => {
                    let record = find_secret(vault.list_secrets().await?, secret)?;
                    match command {
                        SecretCommands::Get { .. } => {
                            println!("{}", serde_json::to_string_pretty(&record)?)
                        }
                        SecretCommands::Update {
                            token_env,
                            no_browser,
                            ..
                        } => {
                            let secret = credential(
                                token_env.as_deref(),
                                record.target.as_ref(),
                                *no_browser,
                                env,
                            )
                            .await?;
                            let record = vault.update_secret(&record.id, secret).await?;
                            println!(
                                "updated secret {} ({}) to revision {}",
                                record.name, record.id, record.revision
                            );
                        }
                        SecretCommands::Delete { .. } => {
                            vault.delete_secret(&record.id).await?;
                            println!("deleted secret {} ({})", record.name, record.id);
                        }
                        _ => unreachable!(),
                    }
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

async fn credential(
    token_env: Option<&str>,
    target: Option<&SecretTarget>,
    no_browser: bool,
    env: &HashMap<String, String>,
) -> Result<Secret> {
    if let Some(variable) = token_env {
        return token(variable, env);
    }
    let Some(SecretTarget::Mcp { server_url }) = target else {
        bail!("OAuth login requires an MCP destination; provide --token-env for other secrets");
    };
    let mut manager = AuthorizationManager::new(server_url.as_str()).await?;
    let challenge = exo_mcp::probe_auth_challenge(server_url).await?;
    let resolution = manager
        .resolve_metadata_from_challenge(challenge.as_deref())
        .await?;
    if !resolution.source.is_discovered() {
        bail!("MCP server does not advertise OAuth; provide a token with --token-env");
    }
    let token_endpoint = resolution.metadata.token_endpoint.clone();
    manager.set_metadata(resolution.metadata);
    let grant = crate::oauth::authorize_with(
        manager,
        &[],
        None,
        |url| crate::oauth::open_browser(url, no_browser),
        Duration::from_secs(300),
    )
    .await?;
    let response = grant
        .credentials
        .token_response
        .context("OAuth token missing")?;
    let expires_at = response
        .expires_in()
        .map(|ttl| {
            grant
                .credentials
                .token_received_at
                .context("OAuth receipt time missing")
                .map(|time| time.saturating_add(ttl.as_secs()))
        })
        .transpose()?;
    let secret = Secret::Oauth {
        access_token: response.access_token().secret().clone(),
        refresh_token: response.refresh_token().map(|token| token.secret().clone()),
        expires_at,
        refresh: Some(exoharness::vault::OAuthRefresh {
            token_endpoint,
            client_id: grant.credentials.client_id,
            resource: grant.resource,
            scopes: grant.credentials.granted_scopes,
        }),
    };
    exoharness::vault::validate_secret(&secret, target)?;
    Ok(secret)
}
