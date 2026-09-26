use super::*;
use crate::{Runtime, SandboxProvider, SendRequest};
use exoharness::{
    BasicExoHarness, BasicExoHarnessConfig, Binding, FileSystemMount, FileSystemMountMode,
    NewThreadRequest, PutSecretRequest, Secret, WriteArtifactRequest,
    vault::{SecretTarget, global_vault},
};
use tempfile::TempDir;

const SOURCE: &str = "---\nname: support-analyst\nharness: basic\nconfig:\n  model: gpt-5.4\n---\n\nInvestigate support tickets.\n";

#[test]
fn explicit_sandbox_keeps_the_harness_preset_image() -> Result<()> {
    let definition = AgentDefinition::parse(
        SOURCE
            .replace("harness: basic", "harness: codex")
            .replace("config:\n", "sandbox:\n  provider: docker\nconfig:\n"),
    )?;
    let config =
        super::config::agent_config(&definition, SandboxProvider::LocalProcess, None, None)?;
    assert_eq!(
        config.sandbox.image.as_deref(),
        Some("exo-codex-sandbox:latest")
    );
    assert!(config.sandbox.enable_networking);
    Ok(())
}

async fn state(config: &BasicExoHarnessConfig) -> Result<Arc<dyn ExoHarness>> {
    let state = Arc::new(BasicExoHarness::in_memory(config.clone()).await?);
    state
        .put_binding(Binding::Llm {
            name: "gpt-5.4".into(),
            model: "gpt-5.4".into(),
            base_url: None,
            secret: None,
        })
        .await?;
    Ok(state)
}

#[tokio::test]
async fn updating_a_definition_preserves_mounts_added_outside_the_spec() -> Result<()> {
    let temp = TempDir::new()?;
    let config = crate::test_support::local_test_config(temp.path().join("state"));
    let store = state(&config).await?;
    let runtime = runtime(store, &config, Default::default())?;
    let definition = AgentDefinition::parse(SOURCE.into())?;
    let agent = runtime.create_managed_agent(&definition, "support").await?;
    let mount = FileSystemMount {
        host_path: temp.path().to_string_lossy().into_owned(),
        mount_path: "/workspace".into(),
        mode: FileSystemMountMode::ReadOnly,
        internal: None,
    };
    let mut config = runtime.get_agent_config(agent.as_ref()).await?;
    config.sandbox.mounts.push(mount.clone());
    runtime.put_agent_config(agent.as_ref(), config).await?;

    let updated = AgentDefinition::parse(SOURCE.replace("Investigate", "Triage"))?;
    runtime.update_managed_agent(&agent, &updated).await?;

    assert_eq!(
        runtime
            .get_agent_config(agent.as_ref())
            .await?
            .sandbox
            .mounts,
        vec![mount]
    );
    runtime.shutdown().await
}

fn runtime(
    state: Arc<dyn ExoHarness>,
    config: &BasicExoHarnessConfig,
    thread: ConversationConfig,
) -> Result<Runtime> {
    Ok(Runtime::new(
        LocalProvider::managed(
            state,
            config.clone(),
            Default::default(),
            Arc::new(cost::PricingTable::empty()),
        )?
        .with_managed_agents(LocalAgentSetup {
            thread,
            ..Default::default()
        }),
        None,
    ))
}

#[tokio::test]
async fn mcp_authentication_errors_identify_the_selected_vaults_and_secret() -> Result<()> {
    use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(401).insert_header("www-authenticate", "Bearer"))
        .mount(&server)
        .await;
    let config = crate::test_support::local_test_config("unused-mcp-auth-test");
    let store = state(&config).await?;
    let vault = store.create_vault("personal").await?;
    let url = format!("{}/mcp", server.uri());
    let secret = vault
        .put_secret(PutSecretRequest {
            name: "github-token".into(),
            target: Some(SecretTarget::mcp(&url)?),
            secret: Secret::Key {
                value: "invalid-github-token".into(),
            },
        })
        .await?;
    global_vault(store.as_ref())
        .await?
        .put_secret(PutSecretRequest {
            name: "github".into(),
            target: Some(SecretTarget::mcp(&format!(
                "{}/different-mcp",
                server.uri()
            ))?),
            secret: Secret::Key {
                value: "wrong-destination-token".into(),
            },
        })
        .await?;
    let definition = AgentDefinition::parse(SOURCE.replace(
        "config:\n",
        &format!("mcp_servers:\n  - type: url\n    name: github\n    url: {url}\nconfig:\n"),
    ))?;
    let runtime = runtime(store.clone(), &config, Default::default())?;
    let agent = runtime.create_managed_agent(&definition, "support").await?;
    for attached in [false, true] {
        let error = runtime
            .open_managed_thread(
                &agent,
                None,
                NewThreadRequest {
                    vaults: if attached {
                        vec![vault.record().id]
                    } else {
                        vec![]
                    },
                    ..Default::default()
                },
            )
            .await
            .err()
            .context("unauthorized MCP must fail")?;
        let message = format!("{error:#}");
        assert!(
            message.contains("MCP server github") && message.contains(&url),
            "{message}"
        );
        if attached {
            assert!(
                message.contains("rejected the vault credential"),
                "{message}"
            );
            assert!(message.contains(&secret.to_string()), "{message}");
            assert!(
                message.contains(&vault.record().id.to_string()),
                "{message}"
            );
            assert!(
                message.contains("selected vaults: [global, personal]"),
                "{message}"
            );
        } else {
            assert!(
                message.contains("no matching vault secret was selected"),
                "{message}"
            );
            assert!(message.contains("selected vaults: [global]"), "{message}");
            assert!(message.contains("attach the vault"), "{message}");
            assert!(!message.contains(&secret.to_string()), "{message}");
        }
        assert!(!message.contains("invalid-github-token"), "{message}");
        assert!(!message.contains("wrong-destination-token"), "{message}");
        assert!(managed::list_threads(agent.as_ref()).await?.is_empty());
    }
    let requests = server.received_requests().await.context("MCP requests")?;
    assert_eq!(requests.len(), 2);
    assert!(!requests[0].headers.contains_key("authorization"));
    assert_eq!(
        requests[1].headers.get("authorization").unwrap(),
        "Bearer invalid-github-token"
    );
    runtime.shutdown().await
}

#[tokio::test]
async fn vault_selection_survives_resume_and_rejects_unsafe_config_changes() -> Result<()> {
    let temp = TempDir::new()?;
    let config = crate::test_support::local_test_config(temp.path().join("state"));
    let store = state(&config).await?;
    let isolated = ConversationConfig {
        sandbox_provider: Some(SandboxProvider::Docker),
        ..Default::default()
    };
    let runtime = runtime(store.clone(), &config, isolated.clone())?;
    let definition = AgentDefinition::parse(SOURCE.into())?;
    let agent = runtime.create_managed_agent(&definition, "support").await?;
    let mount = FileSystemMount {
        host_path: temp.path().to_string_lossy().into_owned(),
        mount_path: "/workspace".into(),
        mode: FileSystemMountMode::ReadOnly,
        internal: None,
    };
    let unsafe_mount = ConversationConfig {
        mounts: vec![mount],
        ..isolated.clone()
    };
    let bad = self::runtime(store.clone(), &config, unsafe_mount.clone())?;
    let error = bad
        .open_managed_thread(&agent, None, Default::default())
        .await
        .err()
        .context("expected protected mount rejection")?;
    assert!(
        error.to_string().contains("exposes vault storage"),
        "{error:#}"
    );
    assert!(managed::list_threads(agent.as_ref()).await?.is_empty());
    let alice = store.create_vault("alice").await?;
    let bob = store.create_vault("bob").await?;
    let thread = runtime
        .open_managed_thread(
            &agent,
            None,
            NewThreadRequest {
                vaults: vec![alice.record().id],
                ..Default::default()
            },
        )
        .await?
        .thread;
    let reference = thread.record().id.to_string();
    let selection = managed::vaults::load_selection(thread.as_ref())
        .await?
        .unwrap();
    assert_eq!(selection.vaults.last().unwrap().id, alice.record().id);
    let vault_versions = |artifacts: Vec<exoharness::ArtifactVersion>| {
        artifacts
            .into_iter()
            .filter(|artifact| artifact.path == "managed-agents/vault.json")
            .count()
    };
    let versions = vault_versions(thread.list_artifacts().await?);
    let reopened = self::runtime(store.clone(), &config, isolated)?;
    reopened
        .open_managed_thread(&agent, Some(&reference), Default::default())
        .await?;
    assert_eq!(
        managed::vaults::load_selection(thread.as_ref()).await?,
        Some(selection)
    );
    assert_eq!(vault_versions(thread.list_artifacts().await?), versions);
    for thread_config in [
        ConversationConfig {
            sandbox_provider: Some(SandboxProvider::LocalProcess),
            ..Default::default()
        },
        unsafe_mount,
    ] {
        let unsafe_runtime = self::runtime(store.clone(), &config, thread_config)?;
        assert!(
            unsafe_runtime
                .open_managed_thread(&agent, Some(&reference), Default::default())
                .await
                .is_err()
        );
        let saved = crate::load_conversation_config(thread.as_ref()).await?;
        assert_eq!(saved.sandbox_provider, Some(SandboxProvider::Docker));
        assert!(saved.mounts.is_empty());
        unsafe_runtime.shutdown().await?;
    }
    assert!(
        runtime
            .open_managed_thread(
                &agent,
                Some(&reference),
                NewThreadRequest {
                    vaults: vec![bob.record().id],
                    ..Default::default()
                }
            )
            .await
            .is_err()
    );
    let mut changed = runtime.get_agent_config(agent.as_ref()).await?;
    changed.harness = crate::AgentHarnessKind::Rlm;
    runtime
        .put_agent_config(agent.as_ref(), changed.clone())
        .await?;
    let error = runtime
        .send(
            agent.clone(),
            thread.clone(),
            SendRequest {
                input: vec![],
                session_id: None,
            },
        )
        .await
        .err()
        .context("changed harness must fail before execution")?;
    assert!(
        error.to_string().contains("thread harness changed"),
        "{error:#}"
    );
    changed.harness = crate::AgentHarnessKind::Basic;
    runtime.put_agent_config(agent.as_ref(), changed).await?;
    agent.write_artifact(WriteArtifactRequest {
        path: managed::AGENT_DEFINITION_PATH.into(),
        contents: SOURCE.replace("config:\n", "mcp_servers:\n  - type: url\n    name: changed\n    url: http://127.0.0.1:1/mcp\nconfig:\n").into_bytes(),
    }).await?;
    let error = runtime
        .send(
            agent.clone(),
            thread,
            SendRequest {
                input: vec![],
                session_id: None,
            },
        )
        .await
        .err()
        .context("changed MCP configuration must fail before execution")?;
    assert!(
        error.to_string().contains("MCP configuration changed"),
        "{error:#}"
    );
    bad.shutdown().await?;
    reopened.shutdown().await?;
    runtime.shutdown().await
}

#[tokio::test]
#[ignore = "requires APFS disk images on macOS or reflink-capable storage on Linux"]
async fn agent_resources_are_inherited_pinned_and_cleaned_up() -> Result<()> {
    let temp = TempDir::new()?;
    let source = temp.path().join("repo");
    std::fs::create_dir(&source)?;
    std::fs::write(source.join("file"), "first")?;
    let config = crate::test_support::local_test_config(temp.path().join("state"));
    let store = state(&config).await?;
    let runtime = runtime(
        store.clone(),
        &config,
        ConversationConfig {
            sandbox_provider: Some(SandboxProvider::Smolvm),
            ..Default::default()
        },
    )?;
    let agent_file = temp.path().join("agent.md");
    let definition_text = SOURCE.replace("config:\n", "resources:\n  - name: code\n    type: directory\n    path: ./repo\n    mount_path: /workspace\nconfig:\n");
    std::fs::write(&agent_file, &definition_text)?;
    let definition = AgentDefinition::load(&agent_file)?;
    let agent = runtime
        .create_managed_agent(&definition, "resources")
        .await?;
    let result = async {
        let first = runtime
            .open_managed_thread(&agent, None, NewThreadRequest::default())
            .await?;
        let first_config = crate::load_conversation_config(first.thread.as_ref()).await?;
        assert_eq!(first_config.resource_mounts.len(), 1);
        assert_eq!(
            crate::conversation_sandbox::conversation_sandbox_spec(
                &crate::load_agent_config(agent.as_ref()).await?,
                &first_config
            )
            .default_workdir,
            "/workspace"
        );
        let first_path = Path::new(&first_config.resource_mounts[0].host_path);
        std::fs::write(first_path.join("file"), "private")?;
        std::fs::write(source.join("file"), "updated source")?;
        runtime.update_managed_agent(&agent, &definition).await?;
        let resumed = runtime
            .open_managed_thread(
                &agent,
                Some(&first.thread.record().slug),
                NewThreadRequest::default(),
            )
            .await?;
        let resumed_config = crate::load_conversation_config(resumed.thread.as_ref()).await?;
        assert_eq!(resumed_config.resources, first_config.resources);
        assert_eq!(std::fs::read_to_string(first_path.join("file"))?, "private");
        let second = runtime
            .open_managed_thread(&agent, None, NewThreadRequest::default())
            .await?;
        let second_config = crate::load_conversation_config(second.thread.as_ref()).await?;
        let second_path = Path::new(&second_config.resource_mounts[0].host_path);
        assert_eq!(
            std::fs::read_to_string(second_path.join("file"))?,
            "updated source"
        );
        assert!(agent.delete_thread(&first.thread.record().id).await?);
        assert!(!first_path.exists());
        assert!(second_path.exists());
        Ok::<_, anyhow::Error>(())
    }
    .await;
    store.delete_agent(&agent.record().id).await?;
    runtime.shutdown().await?;
    result
}

#[tokio::test]
async fn git_resources_require_a_selected_vault_and_matching_origin() -> Result<()> {
    let temp = TempDir::new()?;
    let config = crate::test_support::local_test_config(temp.path().join("state"));
    let store = state(&config).await?;
    let vault = store.create_vault("personal").await?;
    vault
        .put_secret(PutSecretRequest {
            name: "github".into(),
            target: Some(SecretTarget::http("https://different.example")?),
            secret: Secret::Key {
                value: "must-not-leak".into(),
            },
        })
        .await?;
    let runtime = runtime(store.clone(), &config, Default::default())?;
    let definition = AgentDefinition::parse(SOURCE.replace("config:\n", "resources:\n  - name: code\n    type: git_repository\n    url: https://github.com/exoharness/exo\n    credential: github\n    mount_path: /workspace\nconfig:\n"))?;
    let agent = runtime
        .create_managed_agent(&definition, "private-code")
        .await?;
    for vaults in [vec![], vec![vault.record().id]] {
        let error = runtime
            .open_managed_thread(
                &agent,
                None,
                NewThreadRequest {
                    vaults,
                    ..Default::default()
                },
            )
            .await
            .err()
            .context("credential selection should fail")?;
        assert!(!format!("{error:#}").contains("must-not-leak"));
        assert!(
            exo_managed_agents::list_threads(agent.as_ref())
                .await?
                .is_empty()
        );
    }
    runtime.shutdown().await?;
    Ok(())
}
