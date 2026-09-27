use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use std::{
    collections::HashMap,
    process::{Output, Stdio},
    time::Duration,
};
use tokio::process::Command;

pub fn github_cli_command() -> Command {
    let mut command = Command::new("gh");
    command
        .env_remove("GH_TOKEN")
        .env_remove("GITHUB_TOKEN")
        .kill_on_drop(true);
    command
}

async fn output(args: &[&str]) -> Result<Output> {
    tokio::time::timeout(Duration::from_secs(30),
        github_cli_command().args(args).stdin(Stdio::null()).output())
        .await.context("timed out reading GitHub CLI credentials")?
        .context("GitHub login requires GitHub CLI (`gh`) 2.81 or newer on the runtime host, or --client-id for OAuth device login")
}

pub async fn github_cli_account() -> Result<Option<String>> {
    #[derive(Deserialize)]
    struct Account {
        login: String,
        state: String,
    }
    #[derive(Deserialize)]
    struct Status {
        hosts: HashMap<String, Vec<Account>>,
    }
    let output = output(&[
        "auth",
        "status",
        "--active",
        "--hostname",
        "github.com",
        "--json",
        "hosts",
    ])
    .await?;
    ensure!(
        output.status.success(),
        "could not check GitHub CLI login; install gh 2.81 or newer and run `gh auth status --hostname github.com`"
    );
    let mut status: Status = serde_json::from_slice(&output.stdout)
        .context("invalid GitHub CLI login status; install gh 2.81 or newer")?;
    Ok(status
        .hosts
        .remove("github.com")
        .unwrap_or_default()
        .into_iter()
        .find(|account| account.state == "success")
        .map(|account| account.login))
}

pub async fn github_cli_token(account: &str) -> Result<String> {
    let output = output(&[
        "auth",
        "token",
        "--hostname",
        "github.com",
        "--user",
        account,
    ])
    .await?;
    ensure!(
        output.status.success(),
        "GitHub CLI has no token for {account}; run `gh auth login --hostname github.com` for that account (requires gh 2.81 or newer)"
    );
    let value = String::from_utf8(output.stdout)
        .context("invalid GitHub token encoding")?
        .trim()
        .to_owned();
    ensure!(
        !value.is_empty() && value.bytes().all(|c| c.is_ascii_graphic()),
        "GitHub CLI returned an invalid token"
    );
    Ok(value)
}
