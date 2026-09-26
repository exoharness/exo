mod support;

use anyhow::{Context, Result, ensure};
use exo_managed_agents::http::RuntimeClient;
use std::{process::Stdio, time::Duration};
use support::{Fixture, success};
use tokio::io::{AsyncBufReadExt, BufReader};

#[actix_web::test]
async fn serve_exposes_the_shared_client_api_and_isolates_the_selected_agent() -> Result<()> {
    let f = Fixture::new().await?;
    let file = f.agent_file.to_str().context("agent file")?;
    for name in ["served", "other"] {
        f.cli(&["agent", "create", name, "--file", file]).await?;
    }
    let state = f.runtime.exoharness_handle();
    let selected = exo_managed_agents::find_agent(state.as_ref(), "served").await?;
    let other = exo_managed_agents::find_agent(state.as_ref(), "other").await?;
    let mut child = f
        .command(&["serve", "--agent", "served", "--bind", "127.0.0.1:0"])
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()?;
    let mut lines = BufReader::new(child.stdout.take().context("server stdout")?).lines();
    let endpoint = tokio::time::timeout(Duration::from_secs(20), async {
        while let Some(line) = lines.next_line().await? {
            if let Some(url) = line.strip_prefix("listening: ") {
                return Ok::<_, anyhow::Error>(url.to_owned());
            }
        }
        anyhow::bail!("serve exited before listening")
    })
    .await??;
    let client = RuntimeClient::new(&endpoint)?;
    let agents = client.list_agents(None).await?;
    assert_eq!(agents.agents.len(), 1);
    assert_eq!(agents.agents[0].id, selected.record().id);
    let http = reqwest::Client::new();
    for suffix in [
        format!("agent/{}", other.record().id),
        format!("%61gent/{}", other.record().id),
        format!("agent/{}/thread", other.record().id),
        format!("%61gent/{}/thread", other.record().id),
    ] {
        let response = http.get(format!("{endpoint}/{suffix}")).send().await?;
        assert_eq!(response.status(), reqwest::StatusCode::NOT_FOUND);
    }
    let response = http
        .post(format!("{endpoint}/agent"))
        .json(&serde_json::json!({"slug": "forbidden", "name": "Forbidden"}))
        .send()
        .await?;
    assert_eq!(response.status(), reqwest::StatusCode::FORBIDDEN);
    let response = http
        .delete(format!("{endpoint}/agent/{}", selected.record().id))
        .send()
        .await?;
    assert_eq!(response.status(), reqwest::StatusCode::FORBIDDEN);
    let response = http
        .put(format!("{endpoint}/environment"))
        .json(&serde_json::json!({
            "name": "forbidden",
            "config": {"provider": "local_process", "image": "unused"}
        }))
        .send()
        .await?;
    assert_eq!(response.status(), reqwest::StatusCode::FORBIDDEN);
    let response = http
        .delete(format!("{endpoint}/environment/forbidden"))
        .send()
        .await?;
    assert_eq!(response.status(), reqwest::StatusCode::FORBIDDEN);
    let origin = endpoint
        .strip_suffix(exo_managed_agents::http::RUNTIME_PATH)
        .context("runtime path")?;
    assert_eq!(
        http.post(format!("{origin}/request"))
            .send()
            .await?
            .status(),
        reqwest::StatusCode::NOT_FOUND
    );
    f.cli(&["provider", "create", "served-http", "--url", &endpoint])
        .await?;
    let output = success(
        f.output(
            &[
                "--provider",
                "served-http",
                "agent",
                "run",
                "--agent",
                "served",
                "--prompt",
                "Hello over HTTP",
            ],
            None,
            None,
        )
        .await?,
    )?;
    ensure!(output.contains("Workflow reply."), "{output}");
    let slug = support::thread_slug(&output)?;
    let resumed = success(
        f.output(
            &[
                "--provider",
                "served-http",
                "agent",
                "run",
                "--agent",
                "served",
                "--thread",
                slug,
            ],
            None,
            Some("/history\n/quit\n"),
        )
        .await?,
    )?;
    ensure!(resumed.contains("Hello over HTTP"));
    let history = exo_managed_agents::list_threads(selected.as_ref()).await?;
    assert_eq!(history.len(), 1);
    child.kill().await?;
    f.stop().await
}

#[actix_web::test]
async fn serve_rejects_non_loopback_bind_addresses() -> Result<()> {
    let f = Fixture::new().await?;
    let output = f
        .output(&["serve", "--bind", "0.0.0.0:0"], None, None)
        .await?;
    ensure!(!output.status.success());
    ensure!(
        String::from_utf8_lossy(&output.stderr)
            .contains("non-loopback serving requires --auth-file")
    );
    f.stop().await
}
