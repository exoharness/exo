use std::sync::Mutex;

use async_trait::async_trait;
use executor::{
    BasicExoHarness, BasicExoHarnessConfig, BasicHarness, BasicToolRuntime, Binding, ExoHarness,
    ModelClient, ModelRequest, ModelResponse, ModelResponseStream, SandboxBackendRegistration,
    SandboxProvider, SecretBackendChoice, SendRequest,
};
use exoharness::ReadArtifactRequest;
use lingua::universal::{AssistantContent, UserContent};
use tempfile::TempDir;

use super::*;

const SOURCE: &str = "---\nname: support-analyst\nharness: basic\nconfig:\n  model: gpt-5.4\n---\n\nInvestigate support tickets.\n";

#[derive(Default)]
struct RecordingModel {
    requests: Mutex<Vec<ModelRequest>>,
}

#[async_trait]
impl ModelClient for RecordingModel {
    async fn complete(&self, request: ModelRequest) -> Result<ModelResponse> {
        self.requests.lock().unwrap().push(request);
        Ok(ModelResponse {
            response_id: None,
            messages: vec![Message::Assistant {
                content: AssistantContent::String("Ticket analysis saved.".to_string()),
                id: None,
            }],
            tool_calls: Vec::new(),
            usage: None,
            model: None,
            ttft: None,
            duration: None,
            provider_cost_usd: None,
        })
    }

    async fn complete_stream(
        &self,
        _request: ModelRequest,
    ) -> Result<Box<dyn ModelResponseStream>> {
        bail!("this test uses non-streaming sends")
    }
}

async fn harness(root: &Path, model: Arc<RecordingModel>) -> Result<Arc<dyn Harness>> {
    let storage = Arc::new(BasicExoHarness::new(config(root)).await?);
    storage
        .put_binding(Binding::Llm {
            name: "gpt-5.4".to_string(),
            model: "gpt-5.4".to_string(),
            base_url: None,
            secret: None,
        })
        .await?;
    Ok(Arc::new(BasicHarness::new(
        storage,
        model,
        Arc::new(BasicToolRuntime),
    )))
}

fn config(root: &Path) -> BasicExoHarnessConfig {
    BasicExoHarnessConfig {
        root: root.to_path_buf(),
        secret_backend: SecretBackendChoice::Static([7; 32]),
        sandbox_default: SandboxProvider::LocalProcess,
        sandbox_policy: None,
        sandbox_backends: vec![SandboxBackendRegistration::local_process()],
    }
}

fn thread_args(agent: &str) -> ThreadArgs {
    ThreadArgs {
        agent_file: None,
        agent: Some(agent.to_string()),
        thread: None,
        model: None,
        vault: vec![],
        provider: Some(SandboxProviderArg::LocalProcess),
        sandbox_image: None,
        mounts: Vec::new(),
        verbosity: Verbosity::Minimal,
    }
}

async fn temporary_harness(saved: &dyn Harness) -> Result<Arc<dyn Harness>> {
    let storage = BasicExoHarness::in_memory(
        BasicExoHarnessConfig {
            root: PathBuf::new(),
            secret_backend: SecretBackendChoice::Static([0; 32]),
            sandbox_default: SandboxProvider::LocalProcess,
            sandbox_policy: None,
            sandbox_backends: vec![SandboxBackendRegistration::local_process()],
        },
        Some(saved.exoharness_handle().as_ref()),
    )
    .await?;
    Ok(Arc::new(BasicHarness::new(
        Arc::new(storage),
        Arc::new(RecordingModel::default()),
        Arc::new(BasicToolRuntime),
    )))
}

#[tokio::test]
async fn saved_definition_and_turn_history_survive_reopening_without_source() -> Result<()> {
    let temp = TempDir::new()?;
    let source = temp.path().join("agent.md");
    std::fs::write(&source, SOURCE)?;
    let definition = AgentDefinition::load(&source)?;
    let model = Arc::new(RecordingModel::default());
    let storage_root = temp.path().join("state");
    let runtime = harness(&storage_root, Arc::clone(&model)).await?;
    let saved = create_agent(runtime.as_ref(), &definition, "support", None, None).await?;
    assert!(
        create_agent(runtime.as_ref(), &definition, "support", None, None)
            .await
            .is_err()
    );
    let artifacts = saved.exoharness_handle().list_artifacts().await?;
    let artifact = artifacts
        .iter()
        .find(|artifact| artifact.path == "managed-agents/agent.md")
        .unwrap();
    let markdown = saved
        .exoharness_handle()
        .read_artifact(ReadArtifactRequest {
            artifact_id: artifact.artifact_id,
            version: Some(artifact.version),
        })
        .await?
        .unwrap();
    assert_eq!(markdown.contents, SOURCE.as_bytes());
    let (agent, thread) = open_thread(
        runtime.as_ref(),
        None,
        None,
        &thread_args("support"),
        &PreparedMcp::default(),
        &config(&temp.path().join("state")),
    )
    .await?;
    let thread_slug = thread.record().slug.clone();
    thread
        .send(SendRequest {
            input: vec![Message::User {
                content: UserContent::String("Remember ticket 42.".to_string()),
            }],
            session_id: None,
        })
        .await?;
    drop(thread);
    drop(agent);
    drop(saved);
    drop(runtime);
    std::fs::remove_file(source)?;

    let runtime = harness(&storage_root, Arc::clone(&model)).await?;
    let mut args = thread_args("support");
    args.thread = Some(thread_slug);
    let (agent, thread) = open_thread(
        runtime.as_ref(),
        None,
        None,
        &args,
        &PreparedMcp::default(),
        &config(&temp.path().join("state")),
    )
    .await?;
    assert_eq!(agent.list_conversations().await?.len(), 1);
    thread
        .send(SendRequest {
            input: vec![Message::User {
                content: UserContent::String("Which ticket?".to_string()),
            }],
            session_id: None,
        })
        .await?;
    assert_eq!(thread.messages().await?.len(), 4);
    let requests = model.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    for request in requests.iter() {
        assert!(
            matches!(&request.messages[0], Message::System { content: UserContent::String(text) }
            if text == "You are support-analyst.\n\nInvestigate support tickets.")
        );
    }
    assert!(requests[1].messages.iter().any(|message| matches!(message,
        Message::User { content: UserContent::String(text) } if text == "Remember ticket 42.")));
    Ok(())
}

#[tokio::test]
async fn file_runs_use_isolated_memory_and_mounts_stay_on_threads() -> Result<()> {
    let temp = TempDir::new()?;
    let runtime = harness(
        &temp.path().join("state"),
        Arc::new(RecordingModel::default()),
    )
    .await?;
    let definition = AgentDefinition::parse(SOURCE.to_string())?;
    let mut args = thread_args("unused");
    args.agent = None;
    args.agent_file = Some(PathBuf::from("agent.md"));
    args.mounts.push(
        crate::parse_sandbox_mount(&format!("{}:/workspace/tickets:ro", temp.path().display()))
            .unwrap(),
    );
    let first_runtime = temporary_harness(runtime.as_ref()).await?;
    let second_runtime = temporary_harness(runtime.as_ref()).await?;
    let (first, thread) = open_thread(
        first_runtime.as_ref(),
        Some(&definition),
        None,
        &args,
        &PreparedMcp::default(),
        &config(&temp.path().join("state")),
    )
    .await?;
    let (second, _) = open_thread(
        second_runtime.as_ref(),
        Some(&definition),
        None,
        &args,
        &PreparedMcp::default(),
        &config(&temp.path().join("state")),
    )
    .await?;
    assert_ne!(first.record().id, second.record().id);
    assert!(runtime.list_agents().await?.is_empty());
    assert!(
        second_runtime
            .get_agent(&first.record().id.to_string())
            .await?
            .is_none()
    );
    assert!(first.config().await?.sandbox.mounts.is_empty());
    assert_eq!(thread.config().await?.mounts.len(), 1);
    let mut resume = thread_args(&first.record().slug);
    resume.thread = Some("missing".to_string());
    assert!(
        open_thread(
            first_runtime.as_ref(),
            None,
            None,
            &resume,
            &PreparedMcp::default(),
            &config(&temp.path().join("state"))
        )
        .await
        .is_err()
    );
    assert_eq!(first.list_conversations().await?.len(), 1);
    Ok(())
}

#[tokio::test]
async fn unregistered_file_model_uses_registered_default_but_explicit_model_is_strict() -> Result<()>
{
    let temp = TempDir::new()?;
    let runtime = harness(
        &temp.path().join("state"),
        Arc::new(RecordingModel::default()),
    )
    .await?;
    let definition = AgentDefinition::parse(SOURCE.replace("gpt-5.4", "gpt-5.6-sol"))?;
    let mut args = thread_args("unused");
    args.agent = None;
    args.agent_file = Some(PathBuf::from("agent.md"));
    let (agent, thread) = open_thread(
        runtime.as_ref(),
        Some(&definition),
        None,
        &args,
        &PreparedMcp::default(),
        &config(&temp.path().join("state")),
    )
    .await?;
    assert_eq!(agent.config().await?.model, "gpt-5.4");
    assert!(thread.model_override().await?.is_none());
    args.model = Some("gpt-5.4".to_string());
    let temporary = temporary_harness(runtime.as_ref()).await?;
    let (_, explicit_thread) = open_thread(
        temporary.as_ref(),
        Some(&definition),
        None,
        &args,
        &PreparedMcp::default(),
        &config(&temp.path().join("state")),
    )
    .await?;
    assert!(explicit_thread.model_override().await?.is_none());
    args.model = Some("missing".to_string());
    assert!(
        open_thread(
            runtime.as_ref(),
            Some(&definition),
            None,
            &args,
            &PreparedMcp::default(),
            &config(&temp.path().join("state"))
        )
        .await
        .is_err()
    );
    assert_eq!(runtime.list_agents().await?.len(), 1);
    Ok(())
}

#[tokio::test]
async fn vault_selection_survives_resume_and_rejects_unsafe_config_changes() -> Result<()> {
    let temp = TempDir::new()?;
    let runtime = harness(
        &temp.path().join("state"),
        Arc::new(RecordingModel::default()),
    )
    .await?;
    let definition = AgentDefinition::parse(SOURCE.to_owned())?;
    create_agent(
        runtime.as_ref(),
        &definition,
        "support",
        Some(&HarnessSelection::Kind(crate::HarnessKind::Basic)),
        None,
    )
    .await?;
    let mut global_args = thread_args("support");
    global_args.provider = Some(SandboxProviderArg::Docker);
    global_args.mounts.push(
        crate::parse_sandbox_mount(&format!("{}:/workspace", temp.path().display())).unwrap(),
    );
    let error = open_thread(
        runtime.as_ref(),
        None,
        None,
        &global_args,
        &PreparedMcp::default(),
        &config(&temp.path().join("state")),
    )
    .await
    .err()
    .context("expected protected mount rejection")?;
    assert!(error.to_string().contains("exposes vault storage"));
    assert!(
        crate::must_get_agent(runtime.as_ref(), "support")
            .await?
            .list_conversations()
            .await?
            .is_empty()
    );
    let store = runtime.exoharness_handle();
    let vault = store.create_vault("alice").await?;
    store.create_vault("bob").await?;
    let mut args = thread_args("support");
    args.vault = vec!["alice".into()];
    args.provider = Some(SandboxProviderArg::Docker);
    let mut command = Commands::Chat {
        thread: args,
        tui: false,
    };
    let prepared = connect_mcp(runtime.exoharness_handle().as_ref(), None, &mut command).await?;
    let Commands::Chat { thread: args, .. } = command else {
        unreachable!()
    };
    let (_, thread) = open_thread(
        runtime.as_ref(),
        None,
        None,
        &args,
        &prepared,
        &config(&temp.path().join("state")),
    )
    .await?;
    assert_eq!(
        managed::vaults::load_selection(thread.exoharness_handle().as_ref())
            .await?
            .unwrap()
            .vaults
            .last()
            .unwrap()
            .id,
        vault.record().id
    );
    let mut args = thread_args("support");
    args.thread = Some(thread.record().id.to_string());
    args.provider = None;
    let mut command = Commands::Chat {
        thread: args,
        tui: false,
    };
    let prepared = connect_mcp(runtime.exoharness_handle().as_ref(), None, &mut command).await?;
    assert_eq!(
        prepared
            .selection
            .as_ref()
            .unwrap()
            .vaults
            .last()
            .unwrap()
            .id,
        vault.record().id
    );
    let Commands::Chat {
        thread: mut args, ..
    } = command
    else {
        unreachable!()
    };
    args.provider = Some(SandboxProviderArg::LocalProcess);
    assert!(
        open_thread(
            runtime.as_ref(),
            None,
            None,
            &args,
            &prepared,
            &config(&temp.path().join("state"))
        )
        .await
        .is_err()
    );
    assert_eq!(
        thread.config().await?.sandbox_provider,
        Some(SandboxProvider::Docker)
    );
    args.provider = None;
    args.mounts = vec![FileSystemMount {
        host_path: temp.path().to_string_lossy().into_owned(),
        mount_path: "/workspace/host".into(),
        mode: executor::FileSystemMountMode::ReadOnly,
        internal: None,
    }];
    assert!(
        open_thread(
            runtime.as_ref(),
            None,
            None,
            &args,
            &prepared,
            &config(&temp.path().join("state"))
        )
        .await
        .is_err()
    );
    assert!(thread.config().await?.mounts.is_empty());
    args.mounts.clear();
    args.vault = vec!["bob".into()];
    let mut command = Commands::Chat {
        thread: args,
        tui: false,
    };
    assert!(
        connect_mcp(runtime.exoharness_handle().as_ref(), None, &mut command,)
            .await
            .is_err()
    );
    Ok(())
}

#[test]
fn module_harness_paths_are_relative_to_the_agent_file() -> Result<()> {
    let temp = TempDir::new()?;
    let source = temp.path().join("agent.md");
    std::fs::write(
        &source,
        SOURCE.replace("harness: basic", "harness: ./harness.ts"),
    )?;
    let definition = AgentDefinition::load(&source)?;
    assert!(
        matches!(harness_selection(&definition)?, HarnessSelection::TypeScriptModule(path)
        if path == temp.path().join("harness.ts"))
    );
    Ok(())
}
