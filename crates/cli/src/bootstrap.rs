//! First-run setup: storing secrets, registering models, and the
//! interactive bootstrap wizard that runs before the REPL starts.

use std::collections::HashMap;
use std::io::IsTerminal as _;
use std::io::Write as _;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use executor::Runtime;
use exoharness::vault::VaultHandle;
use exoharness::{
    CredentialDestination, CredentialPolicy, ExoHarness, PutSecretRequest, Secret, SecretMetadata,
    UpdateSecretRequest,
};

use crate::managed_agents::check_model_credentials;

/// Default secret name inferred for a model when the agent config names none.
pub fn default_credential_name(model: &str) -> &'static str {
    if model.contains("claude") || model.starts_with("anthropic/") {
        "anthropic"
    } else {
        "openai"
    }
}

/// HTTPS origin that the model credential must be permitted for.
pub fn model_destination(model: &str, base_url: Option<&str>) -> Result<CredentialDestination> {
    let endpoint = exoharness::vault::model_endpoint(base_url, default_credential_variable(model))?;
    CredentialDestination::origin(&endpoint.origin().ascii_serialization())
}

fn default_credential_variable(model: &str) -> &'static str {
    if default_credential_name(model) == "anthropic" {
        "ANTHROPIC_API_KEY"
    } else {
        "OPENAI_API_KEY"
    }
}

/// Combine the allowed destinations of two policies.
pub fn policy_union(
    left: Option<&CredentialPolicy>,
    right: Option<&CredentialPolicy>,
) -> Result<Option<CredentialPolicy>> {
    match (left, right) {
        (None, right) => Ok(right.cloned()),
        (left, None) => Ok(left.cloned()),
        (Some(left), Some(right)) => {
            let mut destinations = match (&left.networking, &right.networking) {
                (
                    exoharness::CredentialNetworkPolicy::Destinations {
                        allowed_destinations: left_destinations,
                    },
                    exoharness::CredentialNetworkPolicy::Destinations {
                        allowed_destinations: right_destinations,
                    },
                ) => {
                    let mut destinations = left_destinations.clone();
                    destinations.extend(right_destinations.iter().cloned());
                    destinations
                }
                _ => bail!(
                    "cannot merge host-allowlist credentials with destination policies; \
                     rebuild the policy with --allow-origin"
                ),
            };
            destinations.dedup();
            Ok(Some(
                CredentialPolicy::destinations(destinations).normalized()?,
            ))
        }
    }
}

/// Add `destination` to a policy's allowed destinations (or create one).
pub fn policy_with_destination(
    policy: Option<&CredentialPolicy>,
    destination: CredentialDestination,
) -> Result<CredentialPolicy> {
    let mut destinations = match policy.map(|policy| &policy.networking) {
        Some(exoharness::CredentialNetworkPolicy::Destinations {
            allowed_destinations,
        }) => allowed_destinations.clone(),
        Some(exoharness::CredentialNetworkPolicy::Limited { .. }) => bail!(
            "credential is restricted to specific hosts; add {} with --allow-origin",
            destination.as_str()
        ),
        None => Vec::new(),
    };
    if !destinations.iter().any(|allowed| allowed == &destination) {
        destinations.push(destination);
    }
    CredentialPolicy::destinations(destinations).normalized()
}

/// Resolve the credential passed to `exo secret set` from --env/--value/stdin.
pub fn resolve_secret_value(
    env: Option<&str>,
    value: Option<&str>,
    loaded_env: &HashMap<String, String>,
    prompt: bool,
) -> Result<String> {
    match (env, value) {
        (Some(env), None) => crate::env_value_from_arg("--env", env, loaded_env),
        (None, Some(value)) => {
            anyhow::ensure!(!value.trim().is_empty(), "secret value must not be empty");
            Ok(value.to_string())
        }
        (Some(_), Some(_)) => bail!("provide either --env or --value, not both"),
        (None, None) if prompt => {
            print!("Enter the secret value (input is not hidden): ");
            std::io::stdout().flush()?;
            let mut line = String::new();
            std::io::stdin().read_line(&mut line)?;
            let value = line.trim().to_string();
            anyhow::ensure!(
                !value.is_empty(),
                "no secret entered; provide --env or --value"
            );
            Ok(value)
        }
        (None, None) => bail!("provide --env or --value"),
    }
}

fn default_vault_name(local: bool) -> &'static str {
    if local { "global" } else { "personal" }
}

async fn ensure_vault(
    store: &dyn ExoHarness,
    name: &str,
    create_missing: bool,
) -> Result<Arc<dyn VaultHandle>> {
    let existing = store
        .list_vaults()
        .await?
        .into_iter()
        .find(|vault| vault.record().name == name);
    match existing {
        Some(vault) => Ok(vault),
        None if create_missing => store
            .create_vault(name)
            .await
            .with_context(|| format!("creating vault {name:?}")),
        None => exo_managed_agents::vaults::find_vault(store, name).await,
    }
}

async fn find_secret_metadata(
    vault: &dyn VaultHandle,
    name: &str,
) -> Result<Option<SecretMetadata>> {
    let id = name.parse::<exoharness::SecretId>().ok();
    Ok(vault
        .list_secrets()
        .await?
        .into_iter()
        .find(|secret| Some(secret.id) == id || secret.name == name))
}

#[derive(Debug, Subcommand)]
pub enum SecretCommands {
    /// Store a credential, creating or updating it in a vault.
    Set {
        /// Secret name, e.g. openai.
        name: String,
        /// Read the credential from this environment variable.
        #[arg(long, value_parser = crate::parse_env_var_name)]
        env: Option<String>,
        /// Supply the credential value directly (prefer --env in shared shells).
        #[arg(long)]
        value: Option<String>,
        /// Target vault (default: global locally, personal remotely).
        #[arg(long)]
        vault: Option<String>,
        /// Permit credential use on this HTTPS origin; repeat for multiple origins.
        #[arg(long)]
        allow_origin: Vec<String>,
    },
    /// List the secrets in a vault.
    List {
        /// Vault to inspect (default: global locally, personal remotely).
        #[arg(long)]
        vault: Option<String>,
    },
}

pub async fn run_secret_command(
    store: &dyn ExoHarness,
    command: &SecretCommands,
    env: &HashMap<String, String>,
    local: bool,
) -> Result<()> {
    match command {
        SecretCommands::Set {
            name,
            env: env_name,
            value,
            vault,
            allow_origin,
        } => {
            anyhow::ensure!(!name.trim().is_empty(), "secret name must not be empty");
            let value = resolve_secret_value(
                env_name.as_deref(),
                value.as_deref(),
                env,
                std::io::stdin().is_terminal(),
            )?;
            let vault_name = vault
                .as_deref()
                .map(str::to_owned)
                .unwrap_or_else(|| default_vault_name(local).to_string());
            let vault = ensure_vault(store, &vault_name, local).await?;
            let mut destinations = Vec::new();
            for origin in allow_origin {
                destinations.push(CredentialDestination::origin(origin)?);
            }
            let policy = (!destinations.is_empty())
                .then(|| CredentialPolicy::destinations(destinations).normalized())
                .transpose()?;
            if let Some(existing) = find_secret_metadata(vault.as_ref(), name).await? {
                let policy = policy_union(existing.policy.as_ref(), policy.as_ref())?;
                let record = vault
                    .update_secret(
                        &existing.id,
                        UpdateSecretRequest {
                            secret: Some(Secret::Key { value }),
                            policy,
                        },
                    )
                    .await?;
                println!(
                    "set secret {} ({}) at revision {}",
                    record.name, record.id, record.revision
                );
            } else {
                let id = vault
                    .put_secret(PutSecretRequest {
                        name: name.clone(),
                        policy,
                        secret: Secret::Key { value },
                    })
                    .await?;
                println!("set secret {name} ({id})");
            }
        }
        SecretCommands::List { vault } => {
            let vault_name = vault
                .as_deref()
                .map(str::to_owned)
                .unwrap_or_else(|| default_vault_name(local).to_string());
            let vault = exo_managed_agents::vaults::find_vault(store, &vault_name).await?;
            crate::print_table(
                &["SECRET", "TYPE", "ID"],
                vault
                    .list_secrets()
                    .await?
                    .into_iter()
                    .map(|secret| {
                        vec![
                            secret.name,
                            match secret.r#type {
                                exoharness::SecretType::Key => "key",
                                exoharness::SecretType::Oauth => "oauth",
                                exoharness::SecretType::GithubCli => "github_cli",
                            }
                            .to_string(),
                            secret.id.to_string(),
                        ]
                    })
                    .collect(),
            )?;
        }
    }
    Ok(())
}

#[derive(Debug, Subcommand)]
pub enum ModelCommands {
    /// Point an agent at a model and the vault secret that authorizes it.
    Register {
        /// Model name, e.g. gpt-5.4.
        model: String,
        /// Vault secret holding the API key.
        #[arg(long)]
        secret: String,
        /// Agent to update; defaults to the only saved agent.
        #[arg(long)]
        agent: Option<String>,
        /// Custom model endpoint.
        #[arg(long)]
        base_url: Option<String>,
    },
}

pub async fn run_model_command(
    runtime: &Runtime,
    store: &dyn ExoHarness,
    command: &ModelCommands,
) -> Result<()> {
    let ModelCommands::Register {
        model,
        secret,
        agent,
        base_url,
    } = command;
    anyhow::ensure!(!model.trim().is_empty(), "model must not be empty");
    let agent = match agent.as_deref() {
        Some(agent) => crate::must_get_agent(runtime, agent).await?,
        None => {
            let agents: Vec<_> = runtime
                .list_agents()
                .await?
                .into_iter()
                .filter(|agent| agent.slug != "__exo_sandbox_cli")
                .collect();
            match agents.as_slice() {
                [agent] => runtime
                    .get_agent(&agent.slug)
                    .await?
                    .with_context(|| format!("agent {} is no longer available", agent.slug))?,
                [] => {
                    bail!("no saved agents; run `exo run <agent.md>` once, or pass --agent <slug>")
                }
                _ => bail!(
                    "multiple saved agents; pass --agent with one of: {}",
                    agents
                        .iter()
                        .map(|agent| agent.slug.clone())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            }
        }
    };
    let reference = exoharness::vault::find_secret(store, secret)
        .await?
        .with_context(|| format!("secret not found: {secret}"))?;
    let vault = exoharness::vault::require_vault(store, &reference.vault_id).await?;
    let existing = find_secret_metadata(vault.as_ref(), secret)
        .await?
        .with_context(|| format!("secret not found: {secret}"))?;
    let destination = model_destination(model, base_url.as_deref())?;
    if !existing
        .policy
        .as_ref()
        .is_some_and(|policy| policy.permits(&destination))
    {
        let policy = policy_with_destination(existing.policy.as_ref(), destination)?;
        vault
            .update_secret(
                &existing.id,
                UpdateSecretRequest {
                    secret: None,
                    policy: Some(policy),
                },
            )
            .await?;
    }
    let mut config = executor::load_agent_config(agent.as_ref()).await?;
    config.model = model.clone();
    config.credential = Some(existing.name.clone());
    if let Some(base_url) = base_url {
        config.base_url = Some(base_url.clone());
    }
    runtime.put_agent_config(agent.as_ref(), config).await?;
    println!(
        "registered model {model} for agent {} using secret {}",
        agent.record().slug,
        existing.name
    );
    Ok(())
}

/// Outcome of checking whether the model is usable for this thread.
pub enum ModelReadiness {
    /// A credential is configured, stored, and permitted for the model endpoint.
    Ready,
    /// The agent names no credential; setup can create one.
    MissingCredential,
    /// A credential is named but unusable; setup can repair it.
    Unusable(anyhow::Error),
}

pub async fn check_model_readiness(
    runtime: &Runtime,
    agent: &dyn executor::AgentHandle,
    thread: &dyn executor::ConversationHandle,
) -> Result<ModelReadiness> {
    let config = runtime.get_thread_agent_config(agent, thread).await?;
    if executor::managed_agents::model_credential_destination(&config)?.is_none() {
        // The harness manages its own model credentials host-side.
        return Ok(ModelReadiness::Ready);
    }
    if config.credential.is_none() {
        return Ok(ModelReadiness::MissingCredential);
    }
    match check_model_credentials(runtime, agent, thread).await {
        Ok(()) => Ok(ModelReadiness::Ready),
        Err(error) => Ok(ModelReadiness::Unusable(error)),
    }
}

/// Print the manual recovery commands for a thread that has no usable model.
pub fn print_setup_guidance(config: &executor::AgentConfig, agent_slug: &str) {
    let secret = config
        .credential
        .clone()
        .unwrap_or_else(|| default_credential_name(&config.model).to_string());
    eprintln!();
    eprintln!("This agent has no usable model credential. Set one up with:");
    eprintln!(
        "  exo secret set {secret} --env {}   # or --value <key>",
        default_credential_variable(&config.model)
    );
    eprintln!(
        "  exo model register {} --secret {secret} --agent {agent_slug}",
        config.model
    );
    eprintln!("Then start the thread again.");
}

/// Run the interactive first-run wizard, then re-check credentials.
///
/// Returns the original error when the setup could not be completed.
pub async fn bootstrap_model(
    runtime: &Runtime,
    store: &dyn ExoHarness,
    agent: &dyn executor::AgentHandle,
    thread: &dyn executor::ConversationHandle,
    readiness: ModelReadiness,
    local: bool,
) -> Result<()> {
    let error = match readiness {
        ModelReadiness::Ready => return Ok(()),
        ModelReadiness::MissingCredential => None,
        ModelReadiness::Unusable(error) => Some(error),
    };
    let config = runtime.get_thread_agent_config(agent, thread).await?;
    let destination = match executor::managed_agents::model_credential_destination(&config) {
        Ok(Some(destination)) => destination,
        _ => {
            return match error {
                Some(error) => Err(error),
                None => bail!(
                    "agent model.credential must name a credential in a selected vault; \
                     add `credential:` to the agent spec's model section"
                ),
            };
        }
    };
    let vault_name = if local { "global" } else { "personal" };
    let secret_name = config
        .credential
        .clone()
        .unwrap_or_else(|| default_credential_name(&config.model).to_string());
    let agent_slug = agent.record().slug.clone();
    let vault = match ensure_vault(store, vault_name, local).await {
        Ok(vault) => vault,
        Err(_) => return finish_unconfigured(error, &config, &agent_slug),
    };
    let existing = find_secret_metadata(vault.as_ref(), &secret_name).await?;
    let policy_ready = existing.as_ref().is_some_and(|existing| {
        existing
            .policy
            .as_ref()
            .is_some_and(|policy| policy.permits(&destination))
    });
    if !policy_ready {
        println!();
        println!(
            "Welcome to exo! This thread's model ({}) is not usable yet.",
            config.model
        );
    }
    match existing {
        Some(existing) => {
            if !policy_ready {
                println!(
                    "Storing secret {secret_name} in vault {vault_name} and permitting {}.",
                    destination.as_str()
                );
                let policy = policy_with_destination(existing.policy.as_ref(), destination)?;
                if let Err(update_error) = vault
                    .update_secret(
                        &existing.id,
                        UpdateSecretRequest {
                            secret: None,
                            policy: Some(policy),
                        },
                    )
                    .await
                {
                    eprintln!("Could not update secret {secret_name}: {update_error:#}");
                    return finish_unconfigured(error, &config, &agent_slug);
                }
            }
        }
        None => {
            println!(
                "A key for {} will be stored as secret {secret_name:?} in vault {vault_name:?}.",
                destination.as_str()
            );
            let value = match resolve_secret_value(None, None, &HashMap::new(), true) {
                Ok(value) => value,
                Err(_) => {
                    println!("No key entered.");
                    return finish_unconfigured(error, &config, &agent_slug);
                }
            };
            if let Err(put_error) = vault
                .put_secret(PutSecretRequest {
                    name: secret_name.clone(),
                    policy: Some(CredentialPolicy::destinations(vec![destination]).normalized()?),
                    secret: Secret::Key { value },
                })
                .await
            {
                eprintln!("Could not store secret {secret_name}: {put_error:#}");
                return finish_unconfigured(error, &config, &agent_slug);
            }
            println!("Stored secret {secret_name}.");
        }
    }
    if config.credential.as_deref() != Some(secret_name.as_str()) {
        let mut config = config;
        config.credential = Some(secret_name);
        runtime.put_agent_config(agent, config).await?;
    }
    println!("Model setup complete. Starting the session...");
    println!();
    check_model_credentials(runtime, agent, thread).await
}

fn finish_unconfigured(
    error: Option<anyhow::Error>,
    config: &executor::AgentConfig,
    agent_slug: &str,
) -> Result<()> {
    print_setup_guidance(config, agent_slug);
    match error {
        Some(error) => Err(error),
        None => bail!(
            "agent model.credential must name a credential in a selected vault; \
             add `credential:` to the agent spec's model section"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_value_requires_exactly_one_source() {
        let env = HashMap::new();
        let error = resolve_secret_value(Some("A"), Some("b"), &env, false)
            .expect_err("both sources should be rejected");
        assert!(error.to_string().contains("not both"));
        let error = resolve_secret_value(None, None, &env, false)
            .expect_err("no source should be rejected");
        assert!(error.to_string().contains("--env or --value"));
    }

    #[test]
    fn secret_value_reads_env_and_direct_value() {
        let mut env = HashMap::new();
        env.insert("EXO_TEST_KEY".to_string(), "loaded".to_string());
        assert_eq!(
            resolve_secret_value(Some("EXO_TEST_KEY"), None, &env, false).unwrap(),
            "loaded"
        );
        assert_eq!(
            resolve_secret_value(None, Some(" direct "), &env, false).unwrap(),
            " direct "
        );
        let error = resolve_secret_value(None, Some("  "), &env, false)
            .expect_err("blank value should be rejected");
        assert!(error.to_string().contains("must not be empty"));
    }

    #[test]
    fn credential_name_follows_the_model_family() {
        assert_eq!(default_credential_name("gpt-5.4"), "openai");
        assert_eq!(default_credential_name("claude-sonnet-4"), "anthropic");
        assert_eq!(default_credential_name("anthropic/claude"), "anthropic");
    }

    #[test]
    fn model_destination_defaults_by_family() {
        let openai = model_destination("gpt-5.4", None).unwrap();
        assert_eq!(openai.as_str(), "https://api.openai.com");
        let anthropic = model_destination("claude-sonnet-4", None).unwrap();
        assert_eq!(anthropic.as_str(), "https://api.anthropic.com");
        let custom = model_destination("gpt-5.4", Some("https://relay.example.com/v1")).unwrap();
        assert_eq!(custom.as_str(), "https://relay.example.com");
    }

    #[test]
    fn policy_merge_adds_missing_destinations() {
        let destination = model_destination("gpt-5.4", None).unwrap();
        let merged = policy_with_destination(None, destination).unwrap();
        assert!(merged.permits(&model_destination("gpt-5.4", None).unwrap()));

        let extra = CredentialDestination::origin("https://relay.example.com").unwrap();
        let existing = CredentialPolicy::destinations(vec![extra.clone()]);
        let destination = model_destination("gpt-5.4", None).unwrap();
        let merged = policy_with_destination(Some(&existing), destination).unwrap();
        assert!(merged.permits(&model_destination("gpt-5.4", None).unwrap()));
        assert!(merged.permits(&extra));
    }
}
