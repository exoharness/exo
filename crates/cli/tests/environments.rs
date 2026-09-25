mod support;

use anyhow::{Context, Result};
use support::{Fixture, thread_slug};

#[actix_web::test]
async fn environments_reconcile_saved_threads_locally_and_over_http() -> Result<()> {
    for provider in ["local", "remote"] {
        let f = Fixture::new().await?;
        f.cli(&["provider", "switch", provider]).await?;
        let workspace = f.temp.path().join("workspace");
        std::fs::create_dir(&workspace)?;
        let mut environment: exoharness::EnvironmentDefinition =
            serde_json::from_value(serde_json::json!({
                "name": "dev", "config": {
                    "provider": "local_process", "image": "unused", "default_workdir": workspace,
                    "enable_networking": true, "idle_seconds": 600,
                }
            }))?;
        let file = f.temp.path().join("environment.yaml");
        std::fs::write(&file, serde_yaml_ng::to_string(&environment)?)?;
        let path = file.to_str().context("environment path")?;
        let rejected = f
            .output(
                &["environment", "create", "other", "--file", path],
                None,
                None,
            )
            .await?;
        assert!(!rejected.status.success());
        assert!(
            String::from_utf8_lossy(&rejected.stderr)
                .contains("environment name in file must match other")
        );
        assert!(
            f.runtime
                .exoharness_handle()
                .list_environments()
                .await?
                .is_empty()
        );
        f.cli(&["environment", "create", "dev", "--file", path])
            .await?;
        assert!(f.cli(&["environment", "list"]).await?.contains("dev"));
        assert!(f.cli(&["environment", "get", "dev"]).await?.contains("600"));
        let agent_file = f.agent_file.to_str().context("agent path")?;
        f.cli(&["agent", "create", "saved", "--file", agent_file])
            .await?;
        let output = f
            .cli(&[
                "agent",
                "run",
                "--agent",
                "saved",
                "--environment",
                "dev",
                "--prompt",
                "first",
            ])
            .await?;
        let slug = thread_slug(&output)?;
        let agent =
            exo_managed_agents::find_agent(f.runtime.exoharness_handle().as_ref(), "saved").await?;
        let thread = exo_managed_agents::find_thread(agent.as_ref(), slug).await?;
        assert_eq!(thread.record().environment.as_ref(), Some(&environment));
        let events = thread.get_events(None).await?.events;
        let sandbox = events
            .iter()
            .find_map(|event| match &event.data {
                exoharness::EventData::SandboxCreated {
                    sandbox_id,
                    default_workdir,
                    idle_seconds,
                    ..
                } => {
                    assert_eq!(default_workdir, workspace.to_str().unwrap());
                    assert_eq!(*idle_seconds, 600);
                    Some(sandbox_id.clone())
                }
                _ => None,
            })
            .context("environment sandbox")?;
        environment.config.image = "updated-image".into();
        environment.config.idle_seconds = Some(900);
        std::fs::write(&file, serde_yaml_ng::to_string(&environment)?)?;
        f.cli(&["environment", "update", "dev", "--file", path])
            .await?;
        std::fs::write(workspace.join("saved-work"), "keep me")?;
        f.cli(&[
            "agent",
            "run",
            "--agent",
            "saved",
            "--thread",
            slug,
            "--environment",
            "dev",
            "--prompt",
            "changed",
        ])
        .await?;
        let resumed = exo_managed_agents::find_thread(agent.as_ref(), slug).await?;
        assert_eq!(resumed.record().id, thread.record().id);
        assert_eq!(resumed.record().environment.as_ref(), Some(&environment));
        let replacement = resumed.list_sandboxes().await?;
        assert_eq!(replacement.len(), 1);
        assert_ne!(replacement[0].id, sandbox);
        let sandbox = replacement[0].id.clone();
        assert_eq!(
            std::fs::read_to_string(workspace.join("saved-work"))?,
            "keep me"
        );
        let resumed_events = resumed.get_events(None).await?.events;
        for event in &events {
            assert!(resumed_events.iter().any(|saved| saved.id == event.id));
        }
        f.cli(&[
            "agent",
            "run",
            "--agent",
            "saved",
            "--thread",
            slug,
            "--environment-file",
            path,
            "--prompt",
            "same environment",
        ])
        .await?;
        let unchanged = exo_managed_agents::find_thread(agent.as_ref(), slug).await?;
        assert_eq!(unchanged.list_sandboxes().await?[0].id, sandbox);
        f.cli(&["environment", "delete", "dev"]).await?;
        assert!(
            !f.cli(&["environment", "list"])
                .await?
                .lines()
                .any(|line| line == "dev")
        );
        f.cli(&[
            "agent", "run", "--agent", "saved", "--thread", slug, "--prompt", "resumed",
        ])
        .await?;
        let thread = exo_managed_agents::find_thread(agent.as_ref(), slug).await?;
        let sandboxes = thread.list_sandboxes().await?;
        assert_eq!(sandboxes.len(), 1);
        assert_eq!(sandboxes[0].id, sandbox);
        let second = f
            .cli(&[
                "agent",
                "run",
                "--agent",
                "saved",
                "--environment-file",
                path,
                "--prompt",
                "new environment",
            ])
            .await?;
        let second = exo_managed_agents::find_thread(agent.as_ref(), thread_slug(&second)?).await?;
        assert_eq!(second.record().environment.as_ref(), Some(&environment));
        assert_ne!(second.list_sandboxes().await?[0].id, sandbox);
        f.cli(&["environment", "create", "dev", "--file", path])
            .await?;
        let file_run = f
            .cli(&[
                "agent",
                "run",
                "--agent-file",
                agent_file,
                "--environment",
                "dev",
                "--prompt",
                "file run",
            ])
            .await?;
        assert!(file_run.contains("Workflow reply."));
        let mut unsupported = environment.clone();
        unsupported.config.policy = Some(exoharness::EgressPolicy {
            allowed_tcp_ports: None,
            networking: exoharness::SandboxNetworkPolicy::Limited {
                allowed_hosts: vec!["example.com".into()],
            },
            credentials: vec![],
        });
        std::fs::write(&file, serde_yaml_ng::to_string(&unsupported)?)?;
        let requests_before = f.model.received_requests().await.unwrap().len();
        let rejected = f
            .output(
                &[
                    "agent",
                    "run",
                    "--agent",
                    "saved",
                    "--environment-file",
                    path,
                    "--prompt",
                    "unsupported network",
                ],
                None,
                None,
            )
            .await?;
        assert!(!rejected.status.success());
        assert!(
            String::from_utf8_lossy(&rejected.stderr)
                .contains("does not support policy.networking.limited")
        );
        assert_eq!(
            f.model.received_requests().await.unwrap().len(),
            requests_before
        );
        f.cli(&["agent", "delete", "saved"]).await?;
        f.stop().await?;
    }
    Ok(())
}
