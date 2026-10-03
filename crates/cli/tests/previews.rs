mod support;

use anyhow::{Context, Result};
use std::{process::Stdio, time::Duration};
use support::Fixture;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
    process::Child,
    time::timeout,
};

async fn setup(provider: &str) -> Result<Fixture> {
    let f = Fixture::new().await?;
    f.cli(&["provider", "switch", provider]).await?;
    f.cli(&[
        "agent",
        "create",
        "dev",
        "--file",
        f.agent_file.to_str().context("agent file")?,
    ])
    .await?;
    let file = f.temp.path().join("environment.yaml");
    std::fs::write(
        &file,
        "name: dev\nconfig:\n  provider: local_process\n  image: unused\n  tcp_ports: [5173, 8000]\npreviews: {}\n",
    )?;
    f.cli(&[
        "environment",
        "create",
        "dev",
        "--file",
        file.to_str().context("environment file")?,
    ])
    .await?;
    Ok(f)
}

async fn open_session(f: &Fixture, thread: &str) -> Result<Child> {
    let mut child = f
        .command(&[
            "agent",
            "run",
            "--agent",
            "dev",
            "--environment",
            "dev",
            "--thread",
            thread,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    child
        .stdin
        .as_mut()
        .context("session stdin")?
        .write_all(b"/help\n")
        .await?;
    let mut stdout = BufReader::new(child.stdout.take().context("session stdout")?).lines();
    timeout(Duration::from_secs(10), async {
        while let Some(line) = stdout.next_line().await? {
            if line == "repl commands:" {
                return Ok::<_, anyhow::Error>(());
            }
        }
        anyhow::bail!("session exited before opening the REPL");
    })
    .await??;
    child.stdout = Some(stdout.into_inner().into_inner());
    Ok(child)
}

async fn close_session(mut child: Child) -> Result<()> {
    child
        .stdin
        .as_mut()
        .context("session stdin")?
        .write_all(b"/quit\n")
        .await?;
    support::success(timeout(Duration::from_secs(10), child.wait_with_output()).await??)?;
    Ok(())
}

async fn previews(f: &Fixture, name: &str) -> Result<executor::PreviewUrls> {
    let agent =
        exo_managed_agents::find_agent(f.runtime.exoharness_handle().as_ref(), "dev").await?;
    let thread = exo_managed_agents::find_thread(agent.as_ref(), name).await?;
    f.runtime
        .preview_urls(agent.as_ref(), thread)
        .await?
        .context("preview URLs")
}

async fn page(previews: &executor::PreviewUrls) -> Result<String> {
    let url = url::Url::parse(&previews.page)?;
    let response = reqwest::Client::new()
        .get(format!("http://127.0.0.1:{}", url.port().context("port")?))
        .header(
            "Host",
            format!(
                "{}:{}",
                url.host_str().context("host")?,
                url.port().unwrap()
            ),
        )
        .send()
        .await?;
    assert_eq!(response.status(), 200);
    let html = response.text().await?;
    assert!(html.contains("Sandbox services"));
    for service in &previews.services {
        assert!(html.contains(&format!("href=\"{}\"", service.url)));
    }
    Ok(html)
}

#[actix_web::test]
async fn inline_previews_keep_their_port_and_live_only_in_their_cli_process() -> Result<()> {
    let f = setup("local").await?;
    let first = f
        .cli(&[
            "agent",
            "run",
            "--agent",
            "dev",
            "--environment",
            "dev",
            "--thread",
            "project",
            "--prompt",
            "Explain the service URLs",
        ])
        .await?;
    assert_eq!(support::thread_slug(&first)?, "project");
    let urls = previews(&f, "project").await?;
    assert_eq!(urls.services.len(), 2);
    assert!(urls.services[0].url.contains("5173.project-"));
    assert!(urls.services[1].url.contains("8000.project-"));
    assert!(urls.page.contains(".localhost:"));
    assert!(first.contains(&urls.page));
    let agent =
        exo_managed_agents::find_agent(f.runtime.exoharness_handle().as_ref(), "dev").await?;
    let thread = exo_managed_agents::find_thread(agent.as_ref(), "project").await?;
    let port = f
        .runtime
        .get_conversation_config(thread.as_ref())
        .await?
        .preview_port
        .context("saved port")?;
    assert!(TcpStream::connect(("127.0.0.1", port)).await.is_err());
    let printed = f.cli(&["thread", "ports", "dev", "project"]).await?;
    assert!(printed.contains(&urls.page));
    assert!(printed.contains("owning Exo process and sandbox services"));
    let requests = f
        .model
        .received_requests()
        .await
        .context("model requests")?;
    let request = requests
        .iter()
        .find(|request| request.url.path() == "/responses")
        .context("model request")?;
    let instructions = std::str::from_utf8(&request.body)?;
    assert!(instructions.contains(&urls.page));
    for service in &urls.services {
        assert!(printed.contains(&service.url));
        assert!(instructions.contains(&service.url));
    }

    let conflict = TcpListener::bind(("127.0.0.1", port)).await?;
    let rejected = f
        .output(
            &["agent", "run", "--agent", "dev", "--thread", "project"],
            None,
            None,
        )
        .await?;
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("binding preview port"));
    assert_eq!(
        f.runtime
            .get_conversation_config(thread.as_ref())
            .await?
            .preview_port,
        Some(port)
    );
    drop(conflict);
    let count_config = |artifacts: Vec<exoharness::ArtifactVersion>| {
        artifacts
            .into_iter()
            .filter(|artifact| artifact.path == "config/executor.json")
            .count()
    };
    let before = count_config(thread.list_artifacts().await?);
    let (first, second) = tokio::try_join!(open_session(&f, "project"), open_session(&f, "other"))?;
    assert_eq!(previews(&f, "project").await?, urls);
    assert_eq!(count_config(thread.list_artifacts().await?), before + 1);
    let other = previews(&f, "other").await?;
    assert_ne!(url::Url::parse(&other.page)?.port(), Some(port));
    page(&urls).await?;
    page(&other).await?;
    close_session(first).await?;
    assert!(TcpStream::connect(("127.0.0.1", port)).await.is_err());
    page(&other).await?;
    close_session(second).await?;

    let file = f.temp.path().join("environment.yaml");
    std::fs::write(
        &file,
        "name: dev\nconfig:\n  provider: local_process\n  image: unused\n  tcp_ports: [5173, 8000]\n",
    )?;
    f.cli(&[
        "environment",
        "update",
        "dev",
        "--file",
        file.to_str().context("environment file")?,
    ])
    .await?;
    let disabled = f
        .cli(&[
            "agent",
            "run",
            "--agent",
            "dev",
            "--environment",
            "dev",
            "--thread",
            "project",
        ])
        .await?;
    assert!(!disabled.contains("sandbox:"));
    let output = f
        .output(&["thread", "ports", "dev", "project"], None, None)
        .await?;
    assert!(!output.status.success());
    f.stop().await
}

#[actix_web::test]
async fn http_provider_owns_one_proxy_after_clients_exit_and_removes_deleted_threads() -> Result<()>
{
    let f = setup("remote").await?;
    let endpoint = f
        .runtime
        .start_preview_server(&f.root, "dev.localhost")
        .await?;
    let (first, second) = tokio::try_join!(open_session(&f, "first"), open_session(&f, "second"))?;
    let first_urls = previews(&f, "first").await?;
    let second_urls = previews(&f, "second").await?;
    assert_ne!(first_urls.page, second_urls.page);
    for urls in [&first_urls, &second_urls] {
        assert_eq!(url::Url::parse(&urls.page)?.port(), Some(endpoint.port));
        assert!(urls.page.contains(".dev.localhost:"));
        page(urls).await?;
    }
    close_session(first).await?;
    page(&first_urls).await?;
    page(&second_urls).await?;
    close_session(second).await?;
    let printed = f.cli(&["thread", "ports", "dev", "first"]).await?;
    assert!(
        printed.contains(&first_urls.page),
        "{printed}\nExpected {}",
        first_urls.page
    );
    let agent =
        exo_managed_agents::find_agent(f.runtime.exoharness_handle().as_ref(), "dev").await?;
    let thread = exo_managed_agents::find_thread(agent.as_ref(), "first").await?;
    assert!(
        f.runtime
            .get_conversation_config(thread.as_ref())
            .await?
            .preview_port
            .is_none()
    );
    f.cli(&[
        "agent",
        "run",
        "--agent",
        "dev",
        "--thread",
        "first",
        "--prompt",
        "Explain the service URLs",
    ])
    .await?;
    let requests = f
        .model
        .received_requests()
        .await
        .context("model requests")?;
    let request = requests
        .iter()
        .find(|request| request.url.path() == "/responses")
        .context("model request")?;
    assert!(std::str::from_utf8(&request.body)?.contains(&first_urls.page));

    f.cli(&["thread", "delete", "dev", "first"]).await?;
    let url = url::Url::parse(&first_urls.page)?;
    assert_eq!(
        reqwest::Client::new()
            .get(format!("http://127.0.0.1:{}", endpoint.port))
            .header(
                "Host",
                format!("{}:{}", url.host_str().unwrap(), endpoint.port)
            )
            .send()
            .await?
            .status(),
        404
    );
    page(&second_urls).await?;
    f.cli(&["agent", "delete", "dev"]).await?;
    let url = url::Url::parse(&second_urls.page)?;
    assert_eq!(
        reqwest::Client::new()
            .get(format!("http://127.0.0.1:{}", endpoint.port))
            .header(
                "Host",
                format!("{}:{}", url.host_str().unwrap(), endpoint.port)
            )
            .send()
            .await?
            .status(),
        404
    );
    f.runtime.shutdown().await?;
    // Cancellation runs on the next scheduler turn.
    tokio::task::yield_now().await;
    assert!(
        TcpStream::connect(("127.0.0.1", endpoint.port))
            .await
            .is_err()
    );
    f.stop().await
}
