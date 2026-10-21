use std::{collections::HashMap, path::Path, sync::Arc};

use anyhow::{Context, Result, ensure};
use exo_managed_agents::{AgentDefinition, vaults::VaultSelection};
use exoharness::{CredentialDestination, PutSecretRequest, vault::VaultHandle};

use super::login::{CredentialArgs, prepare_mcp_login};

pub(super) async fn create(
    vault: &Arc<dyn VaultHandle>,
    path: &Path,
    name: Option<&str>,
    credential: &CredentialArgs,
    env: &HashMap<String, String>,
    local: bool,
) -> Result<()> {
    let mut definition = AgentDefinition::load(path)?;
    if let Some(name) = name {
        definition
            .frontmatter
            .mcp_servers
            .retain(|server| server.name() == name);
    }
    ensure!(
        !definition.frontmatter.mcp_servers.is_empty(),
        "{}",
        match name {
            Some(name) => format!("agent has no MCP server named {name}"),
            None => "agent has no MCP servers".into(),
        }
    );
    let servers = definition.resolve_mcp_servers(&()).await?;
    ensure!(
        !credential.imports_token() || servers.len() == 1,
        "--token-env requires one MCP server; select it with `exo vault secret create VAULT SERVER --from-agent FILE --token-env VARIABLE`"
    );

    println!(
        "Setting up MCP credentials in vault {}...",
        vault.record().name
    );
    for server in servers {
        // Reload metadata after each login: multiple names may share a destination.
        // Match with exactly the same policy rules used when opening a thread.
        let selected =
            VaultSelection::from_vaults(std::slice::from_ref(vault), std::slice::from_ref(&server))
                .await?;
        if selected.bindings[0].secret.is_some() {
            println!("{}: credential already available", server.name);
            continue;
        }
        let target = CredentialDestination::url(&server.url)?;
        let secrets = vault.list_secrets().await?;
        let login = if credential.imports_token() {
            None
        } else {
            match prepare_mcp_login(&server.url)
                .await
                .with_context(|| format!("discovering OAuth for MCP server {}", server.name))?
            {
                Some(login) => Some(login),
                None => {
                    println!("{}: no login required", server.name);
                    continue;
                }
            }
        };
        ensure!(
            !secrets.iter().any(|secret| secret.name == server.name),
            "secret {} already exists in vault {} but does not permit {}; update its policy with `exo vault secret update` or use another vault",
            server.name,
            vault.record().name,
            server.url
        );
        println!("{}: {}", server.name, server.url);
        let args = credential.for_mcp(&server.url);
        let policy = target.into();
        let secret = match login {
            Some(login) => args.read_mcp(login, env, &policy).await,
            None => args
                .read(env, Some(&policy), local)
                .await
                .and_then(|secret| secret.context("MCP token import did not produce a credential")),
        }
        .with_context(|| format!("logging in to MCP server {}", server.name))?;
        let id = vault
            .put_secret(PutSecretRequest {
                name: server.name.clone(),
                policy: Some(policy),
                secret,
            })
            .await?;
        println!("created secret {} ({id})", server.name);
    }
    Ok(())
}
