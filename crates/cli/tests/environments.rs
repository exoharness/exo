mod support;

use anyhow::{Context, Result};
use support::{Fixture, thread_slug};

#[actix_web::test]
async fn named_threads_keep_history_and_explicit_environment_for_local_and_http() -> Result<()> {
    for provider in ["local", "remote"] {
        let f = Fixture::new().await?;
        f.cli(&["provider", "switch", provider]).await?;
        f.cli(&[
            "agent",
            "create",
            "braintrust-dev",
            "--file",
            f.agent_file.to_str().context("agent path")?,
        ])
        .await?;
        let environment_file = f.temp.path().join("environment.yaml");
        let environment: exoharness::EnvironmentDefinition = serde_yaml_ng::from_str(
            "name: local-dev\nconfig:\n  provider: local_process\n  image: unused\n",
        )?;
        std::fs::write(&environment_file, serde_yaml_ng::to_string(&environment)?)?;
        f.cli(&[
            "environment",
            "create",
            "local-dev",
            "--file",
            environment_file.to_str().context("environment path")?,
        ])
        .await?;
        let first = f
            .cli(&[
                "agent",
                "run",
                "--agent",
                "braintrust-dev",
                "--environment",
                "local-dev",
                "--thread",
                "my-project-name",
                "--prompt",
                "first named input",
            ])
            .await?;
        assert_eq!(thread_slug(&first)?, "my-project-name");
        let root = f.runtime.exoharness_handle();
        let agent = exo_managed_agents::find_agent(root.as_ref(), "braintrust-dev").await?;
        let thread = exo_managed_agents::find_thread(agent.as_ref(), "my-project-name").await?;
        assert_eq!(thread.record().environment.as_ref(), Some(&environment));
        let id = thread.record().id;
        let second = f
            .cli(&[
                "agent",
                "run",
                "--agent",
                "braintrust-dev",
                "--environment",
                "local-dev",
                "--thread",
                "my-project-name",
                "--prompt",
                "second named input",
            ])
            .await?;
        assert_eq!(thread_slug(&second)?, "my-project-name");
        let resumed = exo_managed_agents::find_thread(agent.as_ref(), "my-project-name").await?;
        assert_eq!(resumed.record().id, id);
        assert_eq!(resumed.record().environment.as_ref(), Some(&environment));
        assert_eq!(
            exo_managed_agents::list_threads(agent.as_ref())
                .await?
                .len(),
            1
        );
        let history = support::success(
            f.output(
                &[
                    "agent",
                    "run",
                    "--agent",
                    "braintrust-dev",
                    "--thread",
                    "my-project-name",
                ],
                None,
                Some("/history\n/quit\n"),
            )
            .await?,
        )?;
        assert!(
            history.contains("first named input") && history.contains("second named input"),
            "{history}"
        );
        assert_eq!(
            exo_managed_agents::find_thread(agent.as_ref(), "my-project-name")
                .await?
                .record()
                .environment
                .as_ref(),
            Some(&environment)
        );
        f.stop().await?;
    }
    Ok(())
}

#[actix_web::test]
async fn explicit_egress_policy_conflicting_with_environment_is_rejected() -> Result<()> {
    let f = Fixture::new().await?;
    f.cli(&["provider", "switch", "local"]).await?;
    let environment_file = f.temp.path().join("environment.json");
    std::fs::write(
        &environment_file,
        r#"{"name":"dev","config":{"provider":"local_process","image":"unused","policy":{"networking":{"type":"unrestricted"}}}}"#,
    )?;
    let policy_file = f.temp.path().join("egress.json");
    std::fs::write(
        &policy_file,
        r#"{"networking":{"type":"unrestricted"},"allowed_tcp_ports":[443],"credentials":[]}"#,
    )?;
    let output = f
        .output(
            &[
                "agent",
                "run",
                "--agent-file",
                f.agent_file.to_str().context("agent path")?,
                "--environment-file",
                environment_file.to_str().context("environment path")?,
                "--egress-policy",
                policy_file.to_str().context("egress policy path")?,
            ],
            None,
            None,
        )
        .await?;
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("--egress-policy conflicts with the selected environment's policy")
    );
    assert!(
        f.runtime
            .exoharness_handle()
            .list_agents()
            .await?
            .is_empty()
    );

    f.cli(&[
        "agent",
        "create",
        "saved",
        "--file",
        f.agent_file.to_str().context("agent path")?,
    ])
    .await?;
    f.cli(&["thread", "create", "saved", "Saved", "--slug", "saved"])
        .await?;
    f.cli(&[
        "agent",
        "run",
        "--agent",
        "saved",
        "--thread",
        "saved",
        "--environment-file",
        environment_file.to_str().context("environment path")?,
    ])
    .await?;
    let output = f
        .output(
            &[
                "agent",
                "run",
                "--agent",
                "saved",
                "--thread",
                "saved",
                "--egress-policy",
                policy_file.to_str().context("egress policy path")?,
            ],
            None,
            None,
        )
        .await?;
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("--egress-policy conflicts with this thread's saved environment policy")
    );

    let plain_environment = f.temp.path().join("plain-environment.json");
    std::fs::write(
        &plain_environment,
        r#"{"name":"plain","config":{"provider":"local_process","image":"unused"}}"#,
    )?;
    f.cli(&[
        "thread", "create", "saved", "Selected", "--slug", "selected",
    ])
    .await?;
    f.cli(&[
        "agent",
        "run",
        "--agent",
        "saved",
        "--thread",
        "selected",
        "--environment-file",
        plain_environment.to_str().context("environment path")?,
        "--egress-policy",
        policy_file.to_str().context("egress policy path")?,
    ])
    .await?;
    let agent =
        exo_managed_agents::find_agent(f.runtime.exoharness_handle().as_ref(), "saved").await?;
    let thread = exo_managed_agents::find_thread(agent.as_ref(), "selected").await?;
    let selected = executor::load_conversation_config(thread.as_ref()).await?;
    assert_eq!(
        selected.egress_policy.unwrap().allowed_tcp_ports,
        Some(vec![443])
    );
    f.stop().await
}

#[actix_web::test]
async fn thread_mount_commands_reject_environment_overrides_without_mutating_state() -> Result<()> {
    let f = Fixture::new().await?;
    f.cli(&["provider", "switch", "local"]).await?;
    f.cli(&[
        "agent",
        "create",
        "saved",
        "--file",
        f.agent_file.to_str().context("agent path")?,
    ])
    .await?;
    let workspace = f.temp.path().join("workspace");
    std::fs::create_dir(&workspace)?;
    let workspace = std::fs::canonicalize(workspace)?;
    let host = workspace.to_str().context("workspace path")?;
    let environment: exoharness::EnvironmentDefinition =
        serde_json::from_value(serde_json::json!({
            "name": "dev", "config": {
                "provider": "docker", "image": "unused",
                "file_system_mounts": [{
                    "host_path": host, "mount_path": "/workspace", "mode": "rw",
                }],
            }
        }))?;
    let file = f.temp.path().join("environment.json");
    std::fs::write(&file, serde_json::to_string(&environment)?)?;
    let path = file.to_str().context("environment path")?;
    f.cli(&["thread", "create", "saved", "Guarded", "--slug", "guarded"])
        .await?;
    // Opening with EOF applies the environment without starting a sandbox.
    f.cli(&[
        "agent",
        "run",
        "--agent",
        "saved",
        "--thread",
        "guarded",
        "--environment-file",
        path,
    ])
    .await?;
    let agent =
        exo_managed_agents::find_agent(f.runtime.exoharness_handle().as_ref(), "saved").await?;
    let thread = exo_managed_agents::find_thread(agent.as_ref(), "guarded").await?;
    let mounts = environment.config.file_system_mounts.as_ref().unwrap();
    assert_eq!(
        executor::load_conversation_config(thread.as_ref())
            .await?
            .mounts,
        *mounts
    );
    for args in [
        vec![
            "thread", "mount", "create", "saved", "guarded", host, "/extra", "--rw",
        ],
        vec![
            "thread",
            "mount",
            "delete",
            "saved",
            "guarded",
            "/workspace",
        ],
    ] {
        let rejected = f.output(&args, None, None).await?;
        assert!(
            !rejected.status.success(),
            "{args:?} unexpectedly succeeded"
        );
        let error = String::from_utf8_lossy(&rejected.stderr);
        assert!(
            error.contains("mounts are managed by its environment"),
            "{error}"
        );
        assert!(error.contains("--environment-file"), "{error}");
        assert_eq!(
            executor::load_conversation_config(thread.as_ref())
                .await?
                .mounts,
            *mounts
        );
        let unchanged = exo_managed_agents::find_thread(agent.as_ref(), "guarded").await?;
        assert_eq!(unchanged.record().environment.as_ref(), Some(&environment));
    }
    assert!(
        f.cli(&["thread", "mount", "list", "saved", "guarded"])
            .await?
            .contains("/workspace")
    );
    assert!(thread.list_sandboxes().await?.is_empty());

    // Threads without an environment retain mutable mounts.
    f.cli(&["thread", "create", "saved", "Plain", "--slug", "plain"])
        .await?;
    f.cli(&[
        "thread",
        "mount",
        "create",
        "saved",
        "plain",
        host,
        "/workspace",
        "--rw",
    ])
    .await?;
    let plain = exo_managed_agents::find_thread(agent.as_ref(), "plain").await?;
    let config = executor::load_conversation_config(plain.as_ref()).await?;
    assert_eq!(config.mounts.len(), 1);
    assert_eq!(config.mounts[0].host_path, host);
    assert_eq!(config.mounts[0].mount_path, "/workspace");
    f.cli(&["thread", "mount", "delete", "saved", "plain", "/workspace"])
        .await?;
    assert!(
        executor::load_conversation_config(plain.as_ref())
            .await?
            .mounts
            .is_empty()
    );
    f.stop().await?;
    Ok(())
}

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
