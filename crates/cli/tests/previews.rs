mod support;

use anyhow::{Context, Result};
use std::{process::Stdio, time::Duration};
use support::Fixture;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, Command},
    time::{sleep, timeout},
};

async fn open_session(fixture: &Fixture, thread: &str) -> Result<Child> {
    let mut child = fixture
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
        .process_group(0)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut stdout = BufReader::new(child.stdout.take().context("session stdout")?).lines();
    timeout(Duration::from_secs(10), async {
        while let Some(line) = stdout.next_line().await? {
            if line.starts_with("  app (port") {
                return Ok::<_, anyhow::Error>(());
            }
        }
        anyhow::bail!("session exited before assigning previews");
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
    let output = timeout(Duration::from_secs(10), child.wait_with_output()).await??;
    support::success(output)?;
    Ok(())
}

#[actix_web::test]
async fn separate_cli_processes_share_the_proxy_and_close_independently() -> Result<()> {
    let fixture = Fixture::new().await?;
    fixture.cli(&["provider", "switch", "local"]).await?;
    fixture
        .cli(&[
            "agent",
            "create",
            "dev",
            "--file",
            fixture.agent_file.to_str().context("agent file")?,
        ])
        .await?;
    let environment = fixture.temp.path().join("environment.yaml");
    std::fs::write(
        &environment,
        "name: dev\nconfig:\n  provider: local_process\n  image: unused\n  tcp_ports: [5173, 8000, 9000]\npreviews:\n  domain: exo.localhost\n  services:\n    app: 5173\n    api: 8000\n",
    )?;
    fixture
        .cli(&[
            "environment",
            "create",
            "dev",
            "--file",
            environment.to_str().context("environment file")?,
        ])
        .await?;
    let (mut first, second) = tokio::try_join!(
        open_session(&fixture, "first"),
        open_session(&fixture, "second"),
    )?;
    let root = fixture.runtime.exoharness_handle();
    let agent = exo_managed_agents::find_agent(root.as_ref(), "dev").await?;
    let first_thread = exo_managed_agents::find_thread(agent.as_ref(), "first").await?;
    let second_thread = exo_managed_agents::find_thread(agent.as_ref(), "second").await?;
    let first_config = fixture
        .runtime
        .get_conversation_config(first_thread.as_ref())
        .await?;
    let first_previews = first_config.browser_previews;
    let second_previews = fixture
        .runtime
        .get_conversation_config(second_thread.as_ref())
        .await?
        .browser_previews;
    let port = url::Url::parse(&first_previews[0].url)?
        .port()
        .context("preview port")?;
    assert_eq!(first_previews.len(), 3);
    assert!(
        first_previews
            .iter()
            .any(|preview| preview.name == "9000" && preview.port == 9000)
    );
    for preview in first_previews.iter().chain(&second_previews) {
        assert_eq!(url::Url::parse(&preview.url)?.port(), Some(port));
    }
    let portal = |preview: &executor::BrowserPreview| {
        preview.url.split_once("http://app.").unwrap().1.to_owned()
    };
    let first_portal = portal(
        first_previews
            .iter()
            .find(|preview| preview.name == "app")
            .context("first app")?,
    );
    let second_portal = portal(
        second_previews
            .iter()
            .find(|preview| preview.name == "app")
            .context("second app")?,
    );
    let http = reqwest::Client::new();
    let endpoint = format!("http://127.0.0.1:{port}");
    assert_eq!(
        first_config.browser_preview_url,
        Some(format!("http://{first_portal}"))
    );
    for (host, previews) in [
        (&first_portal, &first_previews),
        (&second_portal, &second_previews),
    ] {
        let page = http.get(&endpoint).header("Host", host).send().await?;
        assert_eq!(page.status(), 200);
        let html = page.text().await?;
        assert!(html.contains("Sandbox services"));
        for preview in previews {
            assert!(html.contains(&format!("href=\"{}\"", preview.url)));
            assert!(html.contains(&format!("<td>{}</td>", preview.port)));
        }
    }
    let group = format!("-{}", first.id().context("first session pid")?);
    assert!(
        Command::new("/bin/kill")
            .args(["-TERM", &group])
            .status()
            .await?
            .success()
    );
    assert!(
        !timeout(Duration::from_secs(10), first.wait())
            .await??
            .success()
    );
    timeout(Duration::from_secs(2), async {
        while http
            .get(&endpoint)
            .header("Host", &first_portal)
            .send()
            .await?
            .status()
            != 404
        {
            sleep(Duration::from_millis(10)).await;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    assert_eq!(
        http.get(&endpoint)
            .header("Host", &second_portal)
            .send()
            .await?
            .status(),
        200
    );
    close_session(second).await?;
    fixture.stop().await?;
    Ok(())
}

#[actix_web::test]
async fn named_previews_are_assigned_displayed_and_reused_on_resume() -> Result<()> {
    let fixture = Fixture::new().await?;
    fixture.cli(&["provider", "switch", "local"]).await?;
    fixture
        .cli(&[
            "agent",
            "create",
            "dev",
            "--file",
            fixture.agent_file.to_str().context("agent file")?,
        ])
        .await?;
    let environment = fixture.temp.path().join("environment.yaml");
    std::fs::write(
        &environment,
        "name: dev\nconfig:\n  provider: local_process\n  image: unused\n  tcp_ports: [13000, 8000]\npreviews:\n  domain: exo.localhost\n  services:\n    app: 13000\n    api: 8000\n",
    )?;
    fixture
        .cli(&[
            "environment",
            "create",
            "dev",
            "--file",
            environment.to_str().context("environment file")?,
        ])
        .await?;
    let args = [
        "agent",
        "run",
        "--agent",
        "dev",
        "--environment",
        "dev",
        "--thread",
        "project",
    ];
    let mut initial_args = args.to_vec();
    initial_args.extend(["--prompt", "Explain how to open this sandbox's services."]);
    let first = fixture.cli(&initial_args).await?;
    assert_eq!(support::thread_slug(&first)?, "project");
    let urls = fixture.cli(&["thread", "ports", "dev", "project"]).await?;
    assert!(urls.contains("app.project-"));
    assert!(urls.contains("api.project-"));
    assert!(urls.contains(".dev.exo.localhost:"));
    let root = fixture.runtime.exoharness_handle();
    let agent = exo_managed_agents::find_agent(root.as_ref(), "dev").await?;
    let thread = exo_managed_agents::find_thread(agent.as_ref(), "project").await?;
    let config = fixture
        .runtime
        .get_conversation_config(thread.as_ref())
        .await?;
    let index_url = config.browser_preview_url.context("sandbox services URL")?;
    let previews = config.browser_previews;
    assert!(first.contains(&format!("sandbox: {index_url}")));
    assert!(urls.contains(&index_url));
    let requests = fixture
        .model
        .received_requests()
        .await
        .context("model requests")?;
    let model_request = requests
        .iter()
        .find(|request| request.url.path() == "/responses")
        .context("agent model request")?;
    let model_context = std::str::from_utf8(&model_request.body)?;
    assert!(model_context.contains(&index_url));
    assert_eq!(previews.len(), 2);
    for preview in &previews {
        assert!(first.contains(&preview.url));
        assert!(model_context.contains(&preview.url));
    }
    let session_lock = std::fs::OpenOptions::new().write(true).open(
        fixture
            .root
            .join("previews")
            .join(agent.record().id.to_string())
            .join(thread.record().id.to_string())
            .join("session.lock"),
    )?;
    session_lock.lock()?;
    let sandbox = thread
        .create_sandbox(
            thread
                .record()
                .environment
                .as_ref()
                .context("environment")?
                .config
                .clone(),
        )
        .await?;
    let duplicate = fixture.output(&args, None, Some("/quit\n")).await?;
    assert!(!duplicate.status.success());
    assert!(
        String::from_utf8_lossy(&duplicate.stderr)
            .contains("this thread already has a local preview session")
    );
    assert!(thread.list_sandboxes().await?[0].running);
    thread.terminate_sandbox(sandbox).await?;
    drop(session_lock);
    let resumed = support::success(fixture.output(&args, None, Some("/quit\n")).await?)?;
    for preview in &previews {
        assert!(resumed.contains(&preview.url));
    }
    assert_eq!(
        urls,
        fixture.cli(&["thread", "ports", "dev", "project"]).await?
    );
    fixture.cli(&["provider", "switch", "remote"]).await?;
    let remote = support::success(fixture.output(&args, None, Some("/quit\n")).await?)?;
    for preview in &previews {
        assert!(remote.contains(&preview.url));
    }
    std::fs::write(
        &environment,
        "name: dev\nconfig:\n  provider: local_process\n  image: unused\n  tcp_ports: [13000, 8000]\n",
    )?;
    fixture
        .cli(&[
            "environment",
            "update",
            "dev",
            "--file",
            environment.to_str().context("environment file")?,
        ])
        .await?;
    let disabled = support::success(fixture.output(&args, None, Some("/quit\n")).await?)?;
    assert!(!disabled.contains("sandbox:"));
    let disabled_config = fixture
        .runtime
        .get_conversation_config(thread.as_ref())
        .await?;
    assert!(disabled_config.browser_previews.is_empty());
    assert!(disabled_config.browser_preview_url.is_none());
    fixture.stop().await?;
    Ok(())
}
