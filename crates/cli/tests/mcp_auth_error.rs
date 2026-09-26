use anyhow::Result;
use tempfile::TempDir;
use tokio::process::Command;
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

#[tokio::test]
async fn chat_shows_one_line_for_auth_errors_unless_full_verbosity_is_requested() -> Result<()> {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(401).insert_header("www-authenticate", "Bearer"))
        .mount(&server)
        .await;
    let temp = TempDir::new()?;
    let agent = temp.path().join("github.md");
    std::fs::write(
        &agent,
        format!(
            "---\nname: github-analyst\nharness: codex\nconfig:\n  model: gpt-5.6-sol\nmcp_servers:\n  - type: url\n    name: github\n    url: {}/mcp\n---\nInvestigate GitHub issues.\n",
            server.uri()
        ),
    )?;

    for verbosity in [None, Some("minimal"), Some("full")] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_exo"));
        command
            .env_clear()
            .env("EXO_CONFIG_DIR", temp.path().join("config"))
            .current_dir(temp.path())
            .args(["agent", "run"])
            .arg("--root")
            .arg(temp.path().join("state"))
            .args(["--secret-backend", "file", "--master-key-path"])
            .arg(temp.path().join("master-key"))
            .arg("--agent-file")
            .arg(&agent);
        if let Some(verbosity) = verbosity {
            command.args(["--verbosity", verbosity]);
        }
        let output = command.output().await?;
        assert_eq!(output.status.code(), Some(1));
        let stderr = String::from_utf8(output.stderr)?;
        if verbosity == Some("full") {
            assert!(stderr.contains("Caused by:"), "{stderr}");
            assert!(stderr.contains("selected vaults: [global]"), "{stderr}");
            assert!(stderr.contains("Auth required"), "{stderr}");
        } else {
            assert_eq!(
                stderr.split_once("Error: ").expect("CLI error").1,
                "connecting MCP server github. Add a secret for this MCP server URL to a selected vault, or attach the vault containing it, then start a new thread.\n"
            );
        }
    }
    Ok(())
}
