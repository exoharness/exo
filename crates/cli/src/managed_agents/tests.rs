use std::sync::Mutex;

use async_trait::async_trait;
use executor::{
    BasicExoHarness, BasicExoHarnessConfig, BasicToolRuntime, LocalProvider, ModelClient,
    ModelRequest, ModelResponse, ModelResponseStream, Runtime, SandboxBackendRegistration,
    SandboxProvider, SecretBackendChoice, SendRequest,
};
use exoharness::ReadArtifactRequest;
use lingua::Message;
use lingua::universal::{AssistantContent, UserContent};
use tempfile::TempDir;

use super::*;

const SOURCE: &str = "---\nname: support-analyst\nharness: basic\nconfig:\n  model: gpt-5.4\n  credential: test-openai\n---\n\nInvestigate support tickets.\n";

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

async fn harness(root: &Path, model: Arc<RecordingModel>) -> Result<Arc<Runtime>> {
    let storage = Arc::new(BasicExoHarness::new(storage_config(root)).await?);

    let vault = exoharness::vault::global_vault(storage.as_ref()).await?;
    if vault.list_secrets().await?.is_empty() {
        vault
            .put_secret(exoharness::PutSecretRequest {
                name: "test-openai".into(),
                target: None,
                secret: exoharness::Secret::Key {
                    value: "fixture-key".into(),
                },
            })
            .await?;
    }
    let definition = AgentDefinition::parse(SOURCE.to_string())?;
    Ok(Arc::new(Runtime::new(
        LocalProvider::basic(
            storage,
            model,
            Arc::new(BasicToolRuntime),
            Arc::new(cost::PricingTable::empty()),
        )
        .with_managed_agents(executor::managed_agents::LocalAgentSetup {
            agent: Some(local_agent_config(
                &definition,
                &harness_selection(&definition)?,
                None,
            )?),
            ..Default::default()
        }),
        None,
    )))
}

fn storage_config(root: &Path) -> BasicExoHarnessConfig {
    BasicExoHarnessConfig {
        root: root.to_path_buf(),
        secret_backend: SecretBackendChoice::Static([7; 32]),
        sandbox_default: SandboxProvider::LocalProcess,
        sandbox_policy: None,
        sandbox_backends: vec![SandboxBackendRegistration::local_process()],
    }
}

fn configured_runtime(
    runtime: &Runtime,
    definition: Option<&AgentDefinition>,
    args: &ThreadArgs,
) -> Result<Runtime> {
    let setup = executor::managed_agents::LocalAgentSetup {
        agent: definition
            .map(|definition| local_agent_config(definition, &harness_selection(definition)?, None))
            .transpose()?,
        model: args.model.clone(),
        thread: args.local_config()?,
    };
    Ok(Runtime::new(
        LocalProvider::basic(
            runtime.exoharness_handle(),
            Arc::new(RecordingModel::default()),
            Arc::new(BasicToolRuntime),
            Arc::new(cost::PricingTable::empty()),
        )
        .with_managed_agents(setup),
        None,
    ))
}

async fn open_configured_thread(
    runtime: &Runtime,
    definition: Option<&AgentDefinition>,
    args: &ThreadArgs,
) -> Result<(Arc<dyn AgentHandle>, Arc<dyn ConversationHandle>)> {
    let runtime = configured_runtime(runtime, definition, args)?;
    super::open_thread(&runtime, definition, args).await
}

fn thread_args(agent: &str) -> ThreadArgs {
    ThreadArgs {
        environment: None,
        environment_file: None,
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

#[tokio::test]
async fn saved_definition_and_turn_history_survive_reopening_without_source() -> Result<()> {
    let temp = TempDir::new()?;
    let source = temp.path().join("agent.md");
    std::fs::write(&source, SOURCE)?;
    let definition = AgentDefinition::load(&source)?;
    let model = Arc::new(RecordingModel::default());
    let storage_root = temp.path().join("state");
    let runtime = harness(&storage_root, Arc::clone(&model)).await?;
    let saved = runtime.create_managed_agent(&definition, "support").await?;
    assert!(
        runtime
            .create_managed_agent(&definition, "support")
            .await
            .is_err()
    );
    let artifacts = saved.list_artifacts().await?;
    let artifact = artifacts
        .iter()
        .find(|artifact| artifact.path == "managed-agents/agent.md")
        .unwrap();
    let markdown = saved
        .read_artifact(ReadArtifactRequest {
            artifact_id: artifact.artifact_id,
            version: Some(artifact.version),
        })
        .await?
        .unwrap();
    assert_eq!(markdown.contents, SOURCE.as_bytes());
    let (agent, thread) =
        open_configured_thread(runtime.as_ref(), None, &thread_args("support")).await?;
    let thread_slug = thread.record().slug.clone();
    runtime
        .send(
            Arc::clone(&agent),
            Arc::clone(&thread),
            SendRequest {
                input: vec![Message::User {
                    content: UserContent::String("Remember ticket 42.".to_string()),
                }],
                session_id: None,
            },
        )
        .await?;
    drop(thread);
    drop(agent);
    drop(saved);
    drop(runtime);
    std::fs::remove_file(source)?;

    let runtime = harness(&storage_root, Arc::clone(&model)).await?;
    let mut args = thread_args("support");
    args.thread = Some(thread_slug);
    let (agent, thread) = open_configured_thread(runtime.as_ref(), None, &args).await?;
    assert_eq!(managed::list_threads(agent.as_ref()).await?.len(), 1);
    runtime
        .send(
            Arc::clone(&agent),
            Arc::clone(&thread),
            SendRequest {
                input: vec![Message::User {
                    content: UserContent::String("Which ticket?".to_string()),
                }],
                session_id: None,
            },
        )
        .await?;
    assert_eq!(
        executor::materialize_conversation_messages(&*thread)
            .await?
            .len(),
        4
    );
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
async fn file_runs_reuse_saved_agents_and_mounts_stay_on_threads() -> Result<()> {
    let temp = TempDir::new()?;
    let runtime = harness(
        &temp.path().join("state"),
        Arc::new(RecordingModel::default()),
    )
    .await?;
    let path = temp.path().join("agent.md");
    std::fs::write(&path, SOURCE)?;
    let definition = AgentDefinition::load(&path)?;
    let mut args = thread_args("unused");
    args.agent = None;
    args.agent_file = Some(path);
    args.mounts.push(
        crate::parse_sandbox_mount(&format!("{}:/workspace/tickets:ro", temp.path().display()))
            .unwrap(),
    );
    let (first, thread) =
        open_configured_thread(runtime.as_ref(), Some(&definition), &args).await?;
    let (second, second_thread) =
        open_configured_thread(runtime.as_ref(), Some(&definition), &args).await?;
    assert_eq!(first.record().id, second.record().id);
    assert_ne!(thread.record().id, second_thread.record().id);
    assert_eq!(runtime.list_agents().await?.len(), 1);
    assert!(
        executor::load_agent_config(first.as_ref())
            .await?
            .sandbox
            .mounts
            .is_empty()
    );
    assert_eq!(
        executor::load_conversation_config(&*thread)
            .await?
            .mounts
            .len(),
        1
    );
    let mut resume = thread_args(&first.record().slug);
    resume.thread = Some("missing".to_string());
    assert!(
        open_configured_thread(runtime.as_ref(), None, &resume)
            .await
            .is_err()
    );
    assert_eq!(managed::list_threads(first.as_ref()).await?.len(), 2);
    Ok(())
}

#[tokio::test]
async fn model_names_pass_through_without_registration_or_substitution() -> Result<()> {
    let temp = TempDir::new()?;
    let runtime = harness(
        &temp.path().join("state"),
        Arc::new(RecordingModel::default()),
    )
    .await?;
    let path = temp.path().join("agent.md");
    std::fs::write(&path, SOURCE.replace("gpt-5.4", "gpt-5.6-sol"))?;
    let definition = AgentDefinition::load(&path)?;
    let mut args = thread_args("unused");
    args.agent = None;
    args.agent_file = Some(path);
    let (agent, thread) =
        open_configured_thread(runtime.as_ref(), Some(&definition), &args).await?;
    assert_eq!(
        executor::load_agent_config(agent.as_ref()).await?.model,
        "gpt-5.6-sol"
    );
    assert!(
        executor::get_conversation_model_override(thread.as_ref())
            .await?
            .is_none()
    );
    args.model = Some("another-model".into());
    let (_, explicit) = open_configured_thread(runtime.as_ref(), Some(&definition), &args).await?;
    assert_eq!(
        executor::get_conversation_model_override(explicit.as_ref())
            .await?
            .unwrap()
            .model,
        "another-model"
    );
    args.model = None;
    args.agent_file = None;
    args.agent = Some(agent.record().id.to_string());
    let (_, saved) = open_configured_thread(runtime.as_ref(), None, &args).await?;
    assert!(
        executor::get_conversation_model_override(saved.as_ref())
            .await?
            .is_none()
    );

    Ok(())
}

#[tokio::test]
async fn mcp_authentication_errors_are_concise_unless_verbose() -> Result<()> {
    use exo_mcp::{McpCredentials, McpServerConfig, McpToolSet};
    use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(401).insert_header("www-authenticate", "Bearer"))
        .mount(&server)
        .await;
    for supplied in [false, true] {
        let credentials: std::collections::HashMap<String, String> = if supplied {
            [("github".into(), "invalid-token".into())].into()
        } else {
            Default::default()
        };
        let error = McpToolSet::connect(
            &[McpServerConfig {
                name: "github".into(),
                url: server.uri(),
                allowed_tools: None,
                blocked_tools: vec![],
            }],
            McpCredentials::from(credentials),
        )
        .await
        .err()
        .context("unauthorized MCP must fail")?;
        let mut report = crate::CliError {
            error,
            verbose: true,
        };
        assert!(format!("{report:?}").contains("Caused by:"));
        report.verbose = false;
        let expected = if supplied {
            "connecting MCP server github. The server rejected the token. Check its validity and permissions."
        } else {
            "connecting MCP server github. Add a secret for this MCP server URL to a selected vault, or attach the vault containing it, then start a new thread."
        };
        assert_eq!(format!("{report:?}"), expected);
    }
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
    std::fs::write(temp.path().join("harness.ts"), "export default {};")?;
    std::fs::write(temp.path().join("tools.ts"), "export default {};")?;
    std::fs::write(
        &source,
        SOURCE.replace(
            "harness: basic",
            "harness: ./harness.ts\ntools: [./tools.ts]\ntool_creation: true",
        ),
    )?;
    let definition = AgentDefinition::load(&source)?;
    assert!(
        matches!(harness_selection(&definition)?, HarnessSelection::TypeScriptModule(path)
        if path == temp.path().join("harness.ts"))
    );
    let config = local_agent_config(&definition, &harness_selection(&definition)?, None)?;
    assert!(config.enable_agent_tool_creation);
    assert_eq!(
        config.typescript.unwrap().tool_module_paths,
        vec![
            temp.path()
                .join("tools.ts")
                .canonicalize()?
                .to_string_lossy()
                .into_owned()
        ]
    );
    let basic = AgentDefinition::parse(
        SOURCE.replace("harness: basic", "harness: basic\ntools: [./tools.ts]"),
    )?;
    assert!(local_agent_config(&basic, &harness_selection(&basic)?, None).is_err());
    Ok(())
}
