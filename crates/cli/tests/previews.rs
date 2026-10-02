mod support;

use anyhow::{Context, Result};
use support::Fixture;

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
    let first = support::success(fixture.output(&args, None, Some("/quit\n")).await?)?;
    assert_eq!(support::thread_slug(&first)?, "project");
    let urls = fixture.cli(&["thread", "ports", "dev", "project"]).await?;
    assert!(urls.contains("app.project-"));
    assert!(urls.contains("api.project-"));
    assert!(urls.contains(".dev.exo.localhost:"));
    let root = fixture.runtime.exoharness_handle();
    let agent = exo_managed_agents::find_agent(root.as_ref(), "dev").await?;
    let thread = exo_managed_agents::find_thread(agent.as_ref(), "project").await?;
    let previews = fixture
        .runtime
        .get_conversation_config(thread.as_ref())
        .await?
        .browser_previews;
    assert_eq!(previews.len(), 2);
    for preview in &previews {
        assert!(first.contains(&preview.url));
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
    assert!(!disabled.contains("previews:"));
    assert!(
        fixture
            .runtime
            .get_conversation_config(thread.as_ref())
            .await?
            .browser_previews
            .is_empty()
    );
    fixture.stop().await?;
    Ok(())
}
