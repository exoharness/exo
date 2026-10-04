use super::*;
use crate::{
    AgentHarnessKind, ConversationHarnessConfig, Runtime, SandboxProvider, SendRequest,
    TypeScriptHarnessConfig,
};
use anyhow::Context;
use exoharness::{
    AddEventsRequest, BasicExoHarness, BasicExoHarnessConfig, EventData, FileSystemMount,
    FileSystemMountMode, NewThreadRequest, PutSecretRequest, Secret, WriteArtifactRequest,
    vault::{CredentialDestination, global_vault},
};
use tempfile::TempDir;

const SOURCE: &str = "---\nname: support-analyst\nharness: basic\nconfig:\n  model: gpt-5.4\n---\n\nInvestigate support tickets.\n";

#[test]
fn sandbox_provider_keeps_the_harness_preset_image() -> Result<()> {
    let definition = AgentDefinition::parse(SOURCE.replace("harness: basic", "harness: codex"))?;
    let config = super::config::agent_config(&definition, SandboxProvider::Docker, None, None)?;
    assert_eq!(
        config.sandbox.image.as_deref(),
        Some(
            "ghcr.io/exoharness/codex-devbox@sha256:d6c147fb855862aef256672a81f28ad7970e510bee7c07553b69c282709311f7"
        )
    );
    assert!(config.sandbox.enable_networking);
    Ok(())
}

async fn state(config: &BasicExoHarnessConfig) -> Result<Arc<dyn ExoHarness>> {
    let state = Arc::new(BasicExoHarness::in_memory(config.clone()).await?);

    Ok(state)
}

#[test]
fn thread_harness_image_defaults_respect_explicit_images() -> Result<()> {
    let definition = AgentDefinition::parse(SOURCE.into())?;
    let base = agent_config(&definition, SandboxProvider::Smolvm, None, None)?;
    let pi = ConversationHarnessConfig {
        kind: AgentHarnessKind::TypeScript,
        module_path: Some("/provider/pi-harness.ts".into()),
        preset: Some(TypeScriptHarnessPreset::Pi),
    };
    let codex_image = TypeScriptHarnessPreset::Codex.sandbox_image();
    let pi_image = TypeScriptHarnessPreset::Pi.sandbox_image();
    for (agent_image, thread_image, expected) in [
        (None, None, pi_image),
        (codex_image, None, pi_image),
        (
            Some("custom-agent:latest"),
            None,
            Some("custom-agent:latest"),
        ),
        (
            codex_image,
            Some("custom-thread:latest"),
            Some("custom-thread:latest"),
        ),
    ] {
        let mut config = base.clone();
        config.sandbox.image = agent_image.map(str::to_owned);
        super::config::apply_thread_harness(&mut config, Some(&pi))?;
        let thread = ConversationConfig {
            sandbox_image: thread_image.map(str::to_owned),
            ..Default::default()
        };
        assert_eq!(thread.effective_sandbox_image(&config), expected);
    }
    Ok(())
}

#[test]
fn switching_thread_harnesses_retains_tools_and_rejects_incompatible_harnesses() -> Result<()> {
    let definition = AgentDefinition::parse(SOURCE.into())?;
    let mut config = agent_config(&definition, SandboxProvider::Smolvm, None, None)?;
    config.harness = AgentHarnessKind::TypeScript;
    config.typescript = Some(TypeScriptHarnessConfig {
        module_path: "agent.ts".into(),
        tool_module_paths: vec!["tools.ts".into()],
    });
    let pi = ConversationHarnessConfig {
        kind: AgentHarnessKind::TypeScript,
        module_path: Some("/provider/pi-harness.ts".into()),
        preset: Some(TypeScriptHarnessPreset::Pi),
    };
    super::config::apply_thread_harness(&mut config, Some(&pi))?;
    let selected = config.typescript.clone().context("selected harness")?;
    assert_eq!(selected.module_path, "/provider/pi-harness.ts");
    assert_eq!(selected.tool_module_paths, ["tools.ts"]);

    let basic = ConversationHarnessConfig {
        kind: AgentHarnessKind::Basic,
        module_path: None,
        preset: None,
    };
    let error = super::config::apply_thread_harness(&mut config, Some(&basic)).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("tool modules require a TypeScript harness")
    );
    assert_eq!(config.typescript.as_ref(), Some(&selected));
    Ok(())
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

#[tokio::test]
async fn applying_an_environment_preserves_thread_mounts() -> Result<()> {
    let temp = TempDir::new()?;
    let config = crate::test_support::local_test_config(temp.path().join("state"));
    let store = state(&config).await?;
    let runtime = runtime(store, &config, Default::default())?;
    let definition = AgentDefinition::parse(SOURCE.into())?;
    let agent = runtime.create_managed_agent(&definition, "support").await?;
    let thread = runtime
        .open_managed_thread(
            &agent,
            None,
            NewThreadRequest::default(),
            &Default::default(),
        )
        .await?
        .thread;
    let existing_mount = FileSystemMount {
        host_path: temp.path().to_string_lossy().into_owned(),
        mount_path: "/data".into(),
        mode: FileSystemMountMode::ReadOnly,
        internal: None,
    };
    let mut thread_config = runtime.get_conversation_config(thread.as_ref()).await?;
    thread_config.mounts.push(existing_mount.clone());
    runtime
        .put_conversation_config(thread.as_ref(), thread_config)
        .await?;

    let environment: exoharness::EnvironmentDefinition = serde_json::from_str(
        r#"{"name":"dev","config":{"provider":"local_process","image":"unused","file_system_mounts":[{"host_path":"/host/workspace","mount_path":"/workspace","mode":"rw"}]}}"#,
    )?;
    runtime
        .open_managed_thread(
            &agent,
            Some(&thread.record().id.to_string()),
            NewThreadRequest {
                environment: Some(environment.clone()),
                ..Default::default()
            },
            &Default::default(),
        )
        .await?;
    let mounts = runtime
        .get_conversation_config(thread.as_ref())
        .await?
        .mounts;
    assert_eq!(mounts.len(), 2);
    assert!(mounts.contains(&existing_mount));
    assert!(mounts.iter().any(|mount| mount.mount_path == "/workspace"));

    let mut updated_environment = environment;
    updated_environment.config.file_system_mounts = Some(vec![FileSystemMount {
        host_path: "/host/other".into(),
        mount_path: "/other".into(),
        mode: FileSystemMountMode::ReadWrite,
        internal: None,
    }]);
    runtime
        .open_managed_thread(
            &agent,
            Some(&thread.record().id.to_string()),
            NewThreadRequest {
                environment: Some(updated_environment),
                ..Default::default()
            },
            &Default::default(),
        )
        .await?;
    let mounts = runtime
        .get_conversation_config(thread.as_ref())
        .await?
        .mounts;
    assert_eq!(mounts.len(), 2);
    assert!(mounts.contains(&existing_mount));
    assert!(mounts.iter().any(|mount| mount.mount_path == "/other"));
    runtime.shutdown().await
}

#[tokio::test]
async fn runtime_reads_config_changes_made_by_another_runtime() -> Result<()> {
    let temp = TempDir::new()?;
    let config = crate::test_support::local_test_config(temp.path().join("state"));
    let store = state(&config).await?;
    let server = runtime(store.clone(), &config, Default::default())?;
    let cli = runtime(store, &config, Default::default())?;
    let definition = AgentDefinition::parse(SOURCE.into())?;
    let agent = server.create_managed_agent(&definition, "support").await?;
    let thread = server
        .open_managed_thread(
            &agent,
            None,
            NewThreadRequest::default(),
            &Default::default(),
        )
        .await?
        .thread;

    assert_eq!(
        server.get_agent_config(agent.as_ref()).await?.model,
        "gpt-5.4"
    );
    let mut agent_config = cli.get_agent_config(agent.as_ref()).await?;
    agent_config.model = "new-model".into();
    cli.put_agent_config(agent.as_ref(), agent_config).await?;
    assert_eq!(
        server.get_agent_config(agent.as_ref()).await?.model,
        "new-model"
    );

    assert!(
        server
            .get_conversation_config(thread.as_ref())
            .await?
            .mounts
            .is_empty()
    );
    let mut thread_config = cli.get_conversation_config(thread.as_ref()).await?;
    thread_config.mounts.push(FileSystemMount {
        host_path: temp.path().to_string_lossy().into_owned(),
        mount_path: "/data".into(),
        mode: FileSystemMountMode::ReadOnly,
        internal: None,
    });
    cli.put_conversation_config(thread.as_ref(), thread_config)
        .await?;
    assert_eq!(
        server
            .get_conversation_config(thread.as_ref())
            .await?
            .mounts
            .len(),
        1
    );
    server.shutdown().await?;
    cli.shutdown().await
}

#[tokio::test]
async fn switching_harnesses_keeps_the_thread_history_and_selects_the_new_default_image()
-> Result<()> {
    let temp = TempDir::new()?;
    let config = crate::test_support::local_test_config(temp.path().join("state"));
    let store = state(&config).await?;
    let runtime = runtime(store, &config, Default::default())?;
    let agent = runtime
        .create_managed_agent(&AgentDefinition::parse(SOURCE.into())?, "support")
        .await?;
    let thread = runtime
        .open_managed_thread(&agent, None, Default::default(), &Default::default())
        .await?
        .thread;
    let thread_id = thread.record().id;
    thread
        .add_events(AddEventsRequest {
            session_id: None,
            turn_id: None,
            data: vec![EventData::Messages {
                messages: vec![crate::harness_helpers::user_message("earlier_harness")],
                response_id: None,
                usage: None,
            }],
        })
        .await?;

    let codex = AgentDefinition::parse(SOURCE.replace("harness: basic", "harness: codex"))?;
    runtime.update_managed_agent(&agent, &codex).await?;
    let reopened = runtime
        .open_managed_thread(
            &agent,
            Some(&thread_id.to_string()),
            Default::default(),
            &Default::default(),
        )
        .await?;
    assert_eq!(reopened.thread.record().id, thread_id);
    let codex_config = runtime.get_agent_config(agent.as_ref()).await?;
    let mut thread_config = runtime.get_conversation_config(thread.as_ref()).await?;
    assert_eq!(thread_config.sandbox_image, None);
    assert_eq!(
        thread_config.effective_sandbox_image(&codex_config),
        codex_config.sandbox.image.as_deref()
    );

    // Threads created before this change persisted the preset image as an override.
    thread_config.sandbox_image = Some(format!(
        "ghcr.io/exoharness/codex-devbox@sha256:{}",
        "0".repeat(64)
    ));
    runtime
        .put_conversation_config(thread.as_ref(), thread_config)
        .await?;
    let pi = AgentDefinition::parse(SOURCE.replace("harness: basic", "harness: pi"))?;
    runtime.update_managed_agent(&agent, &pi).await?;
    runtime
        .open_managed_thread(
            &agent,
            Some(&thread_id.to_string()),
            Default::default(),
            &Default::default(),
        )
        .await?;
    let pi_config = runtime.get_agent_config(agent.as_ref()).await?;
    let thread_config = runtime.get_conversation_config(thread.as_ref()).await?;
    assert_eq!(thread_config.sandbox_image, None);
    assert_eq!(
        thread_config.effective_sandbox_image(&pi_config),
        pi_config.sandbox.image.as_deref()
    );
    let mut custom = thread_config;
    custom.sandbox_image = Some("custom-devbox:latest".into());
    runtime
        .put_conversation_config(thread.as_ref(), custom)
        .await?;
    let claude = AgentDefinition::parse(SOURCE.replace("harness: basic", "harness: claude-code"))?;
    runtime.update_managed_agent(&agent, &claude).await?;
    runtime
        .open_managed_thread(
            &agent,
            Some(&thread_id.to_string()),
            Default::default(),
            &Default::default(),
        )
        .await?;
    assert_eq!(
        runtime
            .get_conversation_config(thread.as_ref())
            .await?
            .sandbox_image
            .as_deref(),
        Some("custom-devbox:latest")
    );
    let history = crate::materialize_conversation_messages(thread.as_ref()).await?;
    assert!(matches!(
        history.as_slice(),
        [lingua::Message::User {
            content: lingua::universal::UserContent::String(text)
        }] if text == "earlier_harness"
    ));
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
            policy: Some((CredentialDestination::url(&url)?).into()),
            secret: Secret::Key {
                value: "invalid-github-token".into(),
            },
        })
        .await?;
    global_vault(store.as_ref())
        .await?
        .put_secret(PutSecretRequest {
            name: "github".into(),
            policy: Some(
                (CredentialDestination::url(&format!("{}/different-mcp", server.uri()))?).into(),
            ),
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
                &Default::default(),
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
        .open_managed_thread(&agent, None, Default::default(), &Default::default())
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
            &Default::default(),
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
        .open_managed_thread(
            &agent,
            Some(&reference),
            Default::default(),
            &Default::default(),
        )
        .await?;
    assert_eq!(
        managed::vaults::load_selection(thread.as_ref()).await?,
        Some(selection.clone())
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
                .open_managed_thread(
                    &agent,
                    Some(&reference),
                    Default::default(),
                    &Default::default()
                )
                .await
                .is_err()
        );
        let saved = crate::load_conversation_config(thread.as_ref()).await?;
        assert_eq!(saved.sandbox_provider, Some(SandboxProvider::Docker));
        assert!(saved.mounts.is_empty());
        unsafe_runtime.shutdown().await?;
    }
    let attached = runtime
        .open_managed_thread(
            &agent,
            Some(&reference),
            NewThreadRequest {
                vaults: vec![bob.record().id],
                ..Default::default()
            },
            &Default::default(),
        )
        .await?;
    assert!(attached.thread.record().vaults.contains(&bob.record().id));
    assert_eq!(
        managed::vaults::load_selection(attached.thread.as_ref()).await?,
        Some(selection)
    );
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
            .open_managed_thread(
                &agent,
                None,
                NewThreadRequest::default(),
                &Default::default(),
            )
            .await?;
        let agent_config = crate::load_agent_config(agent.as_ref()).await?;
        let mut first_config = crate::load_conversation_config(first.thread.as_ref()).await?;
        assert!(first_config.resource_mounts.is_empty());
        first_config
            .materialize_resources(first.thread.as_ref(), &agent_config)
            .await?;
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
                &Default::default(),
            )
            .await?;
        let resumed_config = crate::load_conversation_config(resumed.thread.as_ref()).await?;
        assert_eq!(resumed_config.resources, first_config.resources);
        assert_eq!(std::fs::read_to_string(first_path.join("file"))?, "private");
        let second = runtime
            .open_managed_thread(
                &agent,
                None,
                NewThreadRequest::default(),
                &Default::default(),
            )
            .await?;
        let mut second_config = crate::load_conversation_config(second.thread.as_ref()).await?;
        second_config
            .materialize_resources(second.thread.as_ref(), &agent_config)
            .await?;
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
            policy: Some((CredentialDestination::origin("https://different.example")?).into()),
            secret: Secret::Key {
                value: "must-not-leak".into(),
            },
        })
        .await?;
    let runtime = runtime(
        store.clone(),
        &config,
        ConversationConfig {
            sandbox_provider: Some(SandboxProvider::Smolvm),
            ..Default::default()
        },
    )?;
    let definition = AgentDefinition::parse(SOURCE.replace("config:\n", "resources:\n  - name: code\n    type: git_repository\n    url: https://github.com/exoharness/exo\n    credential: github\n    mount_path: /workspace\nconfig:\n"))?;
    let agent = runtime
        .create_managed_agent(&definition, "private-code")
        .await?;
    for vaults in [vec![], vec![vault.record().id]] {
        let opened = runtime
            .open_managed_thread(
                &agent,
                None,
                NewThreadRequest {
                    vaults,
                    ..Default::default()
                },
                &Default::default(),
            )
            .await?;
        let error = runtime
            .send(
                agent.clone(),
                opened.thread.clone(),
                SendRequest {
                    input: vec![],
                    session_id: None,
                },
            )
            .await
            .err()
            .context("credential selection must fail before execution")?;
        assert!(!format!("{error:#}").contains("must-not-leak"));
        assert!(opened.thread.list_sandboxes().await?.is_empty());
        agent.delete_thread(&opened.thread.record().id).await?;
        assert!(
            exo_managed_agents::list_threads(agent.as_ref())
                .await?
                .is_empty()
        );
    }
    runtime.shutdown().await?;
    Ok(())
}
