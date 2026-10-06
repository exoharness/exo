mod support;

use anyhow::{Context, Result};
use std::{process::Stdio, time::Duration};
use support::Fixture;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
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
        "name: dev\nconfig:\n  provider: local_process\n  image: unused\n  tcp_ports: [5173, 8000]\n",
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

async fn open_session_on(f: &Fixture, thread: &str, provider: Option<&str>) -> Result<Child> {
    let agent =
        exo_managed_agents::find_agent(f.runtime.exoharness_handle().as_ref(), "dev").await?;
    let agent_id = agent.record().id.to_string();
    let mut args = Vec::new();
    if let Some(provider) = provider {
        args.extend(["--provider", provider]);
    }
    args.extend([
        "agent",
        "run",
        "--agent",
        agent_id.as_str(),
        "--environment",
        "dev",
        "--thread",
        thread,
    ]);
    let mut child = f
        .command(&args)
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

#[actix_web::test]
async fn server_and_inline_threads_share_a_root_without_sharing_ownership() -> Result<()> {
    let f = setup("remote").await?;
    let remote = open_session_on(&f, "served", Some("remote")).await?;
    let local = open_session_on(&f, "inline", Some("local")).await?;
    let agent =
        exo_managed_agents::find_agent(f.runtime.exoharness_handle().as_ref(), "dev").await?;
    let served = exo_managed_agents::find_thread(agent.as_ref(), "served").await?;
    let agent_id = agent.record().id.to_string();
    let served_id = served.record().id.to_string();
    let sandbox = served
        .create_sandbox(exoharness::test_support::sandbox_request())
        .await?;

    for args in [
        vec![
            "agent",
            "run",
            "--agent",
            agent_id.as_str(),
            "--thread",
            served_id.as_str(),
        ],
        vec!["thread", "delete", agent_id.as_str(), served_id.as_str()],
        vec!["agent", "delete", agent_id.as_str()],
        vec![
            "thread",
            "sandbox",
            "run",
            agent_id.as_str(),
            served_id.as_str(),
            "true",
        ],
    ] {
        let mut local_args = vec!["--provider", "local"];
        local_args.extend(args);
        let rejected = f.output(&local_args, None, None).await?;
        assert!(!rejected.status.success());
        assert!(
            String::from_utf8_lossy(&rejected.stderr).contains("owned by another local process"),
            "{}",
            String::from_utf8_lossy(&rejected.stderr)
        );
        assert!(
            served
                .list_sandboxes()
                .await?
                .iter()
                .find(|s| s.id == sandbox)
                .unwrap()
                .running
        );
    }
    f.cli(&[
        "--provider",
        "local",
        "thread",
        "get",
        agent_id.as_str(),
        served_id.as_str(),
    ])
    .await?;
    close_session(remote).await?;
    assert!(
        served
            .list_sandboxes()
            .await?
            .iter()
            .find(|s| s.id == sandbox)
            .unwrap()
            .running
    );
    // HTTP clients leave ownership with the server, while an inline exit
    // releases only that inline process's thread.
    close_session(local).await?;
    let resumed = f
        .cli(&[
            "--provider",
            "local",
            "agent",
            "run",
            "--agent",
            agent_id.as_str(),
            "--thread",
            "inline",
        ])
        .await?;
    assert_eq!(support::thread_slug(&resumed)?, "inline");
    f.runtime.shutdown().await?;
    assert!(served.list_sandboxes().await?.iter().all(|s| !s.running));
    f.cli(&[
        "--provider",
        "local",
        "thread",
        "sandbox",
        "run",
        agent_id.as_str(),
        served_id.as_str(),
        "true",
    ])
    .await?;
    f.stop().await
}
