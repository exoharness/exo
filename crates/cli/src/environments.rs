use std::path::Path;

use anyhow::{Context, Result, ensure};
use clap::Subcommand;
use exoharness::{EnvironmentDefinition, ExoHarness};

#[derive(Debug, Subcommand)]
pub enum EnvironmentCommands {
    List,
    Get {
        name: String,
    },
    Create {
        name: String,
        #[arg(long)]
        file: std::path::PathBuf,
    },
    Update {
        name: String,
        #[arg(long)]
        file: std::path::PathBuf,
    },
    Delete {
        name: String,
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
