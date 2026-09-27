use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail, ensure};
use clap::{Args, Subcommand, ValueEnum};
use exoharness::vault::SecretReference;
use exoharness::{
    Binding, EnvironmentDefinition, ExoHarness, SandboxProvider, SandboxProviderConfig,
    default_aws_agentcore_image, default_daytona_image, default_docker_image, default_e2b_template,
    default_firecracker_image, default_vercel_image,
};

#[derive(Debug, Subcommand)]
pub enum EnvironmentCommands {
    /// List saved environments.
    List,
    /// Show an environment definition.
    Get { name: String },
    /// Save an environment from a YAML file.
    Create {
        name: String,
        #[arg(long)]
        file: PathBuf,
    },
    /// Replace a saved environment from a YAML file.
    Update {
        name: String,
        #[arg(long)]
        file: PathBuf,
    },
    /// Delete a saved environment definition.
    Delete { name: String },
    /// Configure environment backends and their vault credentials.
    Provider {
        #[command(subcommand)]
        command: ProviderCommands,
    },
}

pub fn load(path: &Path) -> Result<EnvironmentDefinition> {
    let source = std::fs::read_to_string(path)
        .with_context(|| format!("reading environment {}", path.display()))?;
    let definition: EnvironmentDefinition =
        serde_yaml_ng::from_str(&source).context("invalid environment definition")?;
    definition.validate()?;
    Ok(definition)
}

pub async fn find(state: &dyn ExoHarness, name: &str) -> Result<EnvironmentDefinition> {
    state
        .list_environments()
        .await?
        .into_iter()
        .find(|env| env.name == name)
        .with_context(|| format!("environment not found: {name}"))
}

pub async fn run(state: &dyn ExoHarness, command: EnvironmentCommands) -> Result<()> {
    match command {
        EnvironmentCommands::Provider { command } => configure_provider(state, command).await?,
        EnvironmentCommands::List => {
            for environment in state.list_environments().await? {
                println!("{}", environment.name);
            }
        }
        EnvironmentCommands::Get { name } => {
            println!("{}", serde_yaml_ng::to_string(&find(state, &name).await?)?)
        }
        EnvironmentCommands::Create { name, file } => save(state, &name, &file, false).await?,
        EnvironmentCommands::Update { name, file } => save(state, &name, &file, true).await?,
        EnvironmentCommands::Delete { name } => {
            ensure!(
                state.delete_environment(&name).await?,
                "environment not found: {name}"
            );
        }
    }
    Ok(())
}

async fn save(state: &dyn ExoHarness, name: &str, file: &Path, update: bool) -> Result<()> {
    let environment = load(file)?;
    ensure!(
        environment.name == name,
        "environment name in file must match {name}"
    );
    let exists = state
        .list_environments()
        .await?
        .iter()
        .any(|env| env.name == name);
    ensure!(
        exists == update,
        if update {
            "environment does not exist; use create"
        } else {
            "environment already exists; use update"
        }
    );
    state.put_environment(environment).await
}

#[derive(Debug, Subcommand)]
pub enum ProviderCommands {
    /// List configured environment backends.
    List,
    /// Configure an environment backend and its vault credential.
    #[command(alias = "configure")]
    Create(Box<ProviderConfigureArgs>),
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum BackendArg {
    Daytona,
    #[value(name = "e2b")]
    E2b,
    Sprites,
    Vercel,
    #[value(name = "aws-agentcore")]
    AwsAgentCore,
    Docker,
    Smolvm,
    Firecracker,
}

impl From<BackendArg> for SandboxProvider {
    fn from(value: BackendArg) -> Self {
        match value {
            BackendArg::Daytona => Self::Daytona,
            BackendArg::E2b => Self::E2b,
            BackendArg::Sprites => Self::Sprites,
            BackendArg::Vercel => Self::Vercel,
            BackendArg::AwsAgentCore => Self::AwsAgentCore,
            BackendArg::Docker => Self::Docker,
            BackendArg::Smolvm => Self::Smolvm,
            BackendArg::Firecracker => Self::Firecracker,
        }
    }
}

#[derive(Debug, Args)]
pub struct ProviderConfigureArgs {
    #[arg(long = "backend", value_enum)]
    provider: BackendArg,
    /// Binding name (default: the provider name).
    #[arg(long)]
    name: Option<String>,
    /// Secret (by name) holding the provider's API key/token. Required for remote providers.
    #[arg(long)]
    secret: Option<String>,
    /// Vault containing the backend credential.
    #[arg(long, default_value = "global")]
    vault: String,
    /// Region/target. Daytona: us | eu | experimental.
    #[arg(long)]
    region: Option<String>,
    /// Daytona organization id, or Sprites organization slug.
    #[arg(long)]
    organization_id: Option<String>,
    #[arg(long)]
    project_id: Option<String>,
    #[arg(long)]
    api_url: Option<String>,
    #[arg(long = "runtime-arn")]
    runtime_arn: Option<String>,
    #[arg(long)]
    qualifier: Option<String>,
    /// AgentCore managed session storage mount path configured on the runtime.
    #[arg(long = "session-storage-mount-path")]
    session_storage_mount_path: Option<String>,
    /// Default base image for sandboxes that don't request one.
    #[arg(long)]
    default_image: Option<String>,
    /// Path to the SmolVM executable; omit to use the installed runtime.
    #[arg(long = "smolvm-binary", env = "SMOLVM_BIN")]
    smolvm_binary: Option<PathBuf>,
    /// smolvm: binary to exec when booting a VM, for installs where the entry
    /// point is a wrapper script. Defaults to the `smolvm-bin` beside
    /// `--smolvm-binary`, else that binary itself.
    #[arg(long = "smolvm-boot-binary", env = "SMOLVM_BOOT_BINARY")]
    smolvm_boot_binary: Option<PathBuf>,
    /// Sprites sprite HTTP URL auth: sprite | public.
    #[arg(long)]
    url_auth: Option<String>,
    /// Extra Sprites labels (repeatable). Exo resume labels are added on create.
    #[arg(long = "label")]
    labels: Vec<String>,
}

async fn configure_provider(state: &dyn ExoHarness, command: ProviderCommands) -> Result<()> {
    match command {
        ProviderCommands::List => {
            for record in state.list_bindings().await? {
                if let Binding::Sandbox { name, config } = record.binding {
                    println!("{name}\t{config:?}");
                }
            }
        }
        ProviderCommands::Create(args) => {
            let ProviderConfigureArgs {
                provider,
                name,
                secret,
                vault,
                region,
                organization_id,
                project_id,
                api_url,
                runtime_arn,
                qualifier,
                session_storage_mount_path,
                default_image,
                smolvm_binary,
                smolvm_boot_binary,
                url_auth,
                labels,
            } = *args;
            let binding_name =
                name.unwrap_or_else(|| SandboxProvider::from(provider).as_str().to_string());
            if !matches!(provider, BackendArg::AwsAgentCore) && session_storage_mount_path.is_some()
            {
                bail!("--session-storage-mount-path is only valid for aws-agentcore");
            }
            let config = match provider {
                BackendArg::Daytona => {
                    let secret_id = required_secret(state, &vault, secret, "daytona").await?;
                    SandboxProviderConfig::Daytona {
                        api_key_secret: secret_id,
                        region,
                        organization_id,
                        api_url,
                        default_image: default_image.unwrap_or_else(default_daytona_image),
                    }
                }
                BackendArg::Vercel => {
                    let secret_id = required_secret(state, &vault, secret, "vercel").await?;
                    let team_id = organization_id
                        .ok_or_else(|| anyhow!("--organization-id is required for vercel"))?;
                    let project_id =
                        project_id.ok_or_else(|| anyhow!("--project-id is required for vercel"))?;
                    SandboxProviderConfig::Vercel {
                        api_token_secret: secret_id,
                        team_id,
                        project_id,
                        api_url,
                        default_image: default_image.unwrap_or_else(default_vercel_image),
                    }
                }
                BackendArg::AwsAgentCore => {
                    let runtime_arn = runtime_arn
                        .ok_or_else(|| anyhow!("--runtime-arn is required for aws-agentcore"))?;
                    let region = match region {
                            Some(region) => region,
                            None => aws_region_from_arn(&runtime_arn, "bedrock-agentcore").ok_or_else(|| {
                                anyhow!(
                                    "--region is required when the AgentCore runtime ARN does not include a region"
                                )
                            })?,
                        };
                    SandboxProviderConfig::AwsAgentCore {
                        runtime_arn,
                        region,
                        qualifier,
                        endpoint_url: api_url,
                        session_storage_mount_path,
                        default_image: default_image.unwrap_or_else(default_aws_agentcore_image),
                    }
                }
                BackendArg::Docker => SandboxProviderConfig::Docker {
                    default_image: default_image.unwrap_or_else(default_docker_image),
                },
                BackendArg::Smolvm => SandboxProviderConfig::Smolvm {
                    default_image: default_image.unwrap_or_else(default_docker_image),
                    binary: smolvm_binary,
                    boot_binary: smolvm_boot_binary,
                },
                BackendArg::Firecracker => SandboxProviderConfig::Firecracker {
                    default_image: default_image.unwrap_or_else(default_firecracker_image),
                },
                BackendArg::E2b => {
                    let secret_id = required_secret(state, &vault, secret, "e2b").await?;
                    SandboxProviderConfig::E2b {
                        api_key_secret: secret_id,
                        api_url,
                        default_image: default_image.unwrap_or_else(default_e2b_template),
                    }
                }
                BackendArg::Sprites => {
                    let secret_id = required_secret(state, &vault, secret, "sprites").await?;
                    SandboxProviderConfig::Sprites {
                        token_secret: secret_id,
                        api_url,
                        url_auth,
                        organization: organization_id,
                        labels,
                    }
                }
            };
            let id = state
                .put_binding(Binding::Sandbox {
                    name: binding_name.clone(),
                    config,
                })
                .await?;
            println!("configured environment provider {binding_name} ({id})");
        }
    }
    Ok(())
}

async fn required_secret(
    state: &dyn ExoHarness,
    vault: &str,
    secret: Option<String>,
    backend: &str,
) -> Result<SecretReference> {
    let name = secret.with_context(|| format!("--secret is required for {backend}"))?;
    let vault = exo_managed_agents::vaults::find_vault(state, vault).await?;
    let secret = vault
        .list_secrets()
        .await?
        .into_iter()
        .rev()
        .find(|secret| secret.name == name)
        .with_context(|| format!("secret not found: {name}"))?;
    Ok(SecretReference {
        vault_id: vault.record().id,
        secret_id: secret.id,
    })
}

fn aws_region_from_arn(resource_arn: &str, expected_service: &str) -> Option<String> {
    let mut parts = resource_arn.split(':');
    let arn = parts.next()?;
    let _partition = parts.next()?;
    let service = parts.next()?;
    let region = parts.next()?;
    if arn == "arn" && service == expected_service && !region.is_empty() {
        return Some(region.to_string());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn environment_file_defaults_to_smolvm() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let file = directory.path().join("environment.yaml");
        std::fs::write(
            &file,
            "name: support-analyst-env\nconfig:\n  image: ghcr.io/exoharness/codex-sandbox:latest\n",
        )?;
        let environment = load(&file)?;
        assert_eq!(environment.config.provider, SandboxProvider::Smolvm);
        Ok(())
    }
}
