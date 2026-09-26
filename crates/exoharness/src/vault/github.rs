use anyhow::{Context, Result, ensure};
use std::{process::Stdio, time::Duration};

pub async fn github_cli_token(account: &str) -> Result<String> {
    let output = tokio::time::timeout(
        Duration::from_secs(30),
        tokio::process::Command::new("gh")
            .args([
                "auth",
                "token",
                "--hostname",
                "github.com",
                "--user",
                account,
            ])
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .context("timed out reading the GitHub CLI credential")?
    .context("reading the GitHub CLI credential requires `gh` on the runtime host")?;
    ensure!(
        output.status.success(),
        "GitHub CLI has no token for {account}; run `gh auth login --hostname github.com` for that account"
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
