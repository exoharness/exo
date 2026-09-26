use std::{net::TcpListener, sync::Arc, time::Duration};

use anyhow::{Result, bail};
use async_trait::async_trait;
use executor::{
    AgentHarnessKind, BasicExoHarness, BasicExoHarnessConfig, BasicToolRuntime, CreateAgentRequest,
    LocalProvider, ModelClient, ModelRequest, ModelResponse, ModelResponseStream, Runtime,
    SandboxBackendRegistration, SandboxProvider, SecretBackendChoice,
    http_service::{RuntimeHttpService, server},
};
use exoharness::{AddEventsRequest, EventData, UsageRecord};
use lingua::{
    Message, UniversalStreamChunk, UniversalUsage,
    universal::{AssistantContent, UserContent},
};
use tempfile::TempDir;
use tokio::process::Command;

struct MeteredModel;
struct MeteredStream(ModelResponse);

#[async_trait]
impl ModelResponseStream for MeteredStream {
    async fn next_chunk(&mut self) -> Result<Option<UniversalStreamChunk>> {
        Ok(None)
    }
    async fn finish(self: Box<Self>) -> Result<ModelResponse> {
        Ok(self.0)
    }
}

#[async_trait]
impl ModelClient for MeteredModel {
    async fn complete(&self, request: ModelRequest) -> Result<ModelResponse> {
        let Some(Message::User {
            content: UserContent::String(input),
        }) = request.messages.last()
        else {
            bail!("missing test input");
        };
        Ok(ModelResponse {
            response_id: None,
            messages: vec![Message::Assistant {
                content: AssistantContent::String("Measured response.".into()),
                id: None,
            }],
            tool_calls: vec![],
            usage: (!matches!(input.as_str(), "missing" | "cost-only")).then(|| UniversalUsage {
                prompt_tokens: Some(1000),
                completion_tokens: Some(100),
                prompt_cached_tokens: Some(800),
                completion_reasoning_tokens: Some(20),
                ..Default::default()
            }),
            model: Some(
                if input == "unpriced" {
                    "unpriced-model"
                } else {
                    "metered-model"
                }
                .into(),
            ),
            ttft: None,
            duration: None,
            provider_cost_usd: match input.as_str() {
                "priced" => Some(0.01),
                "cost-only" => Some(0.03),
                _ => None,
            },
        })
    }

    async fn complete_stream(&self, request: ModelRequest) -> Result<Box<dyn ModelResponseStream>> {
        Ok(Box::new(MeteredStream(self.complete(request).await?)))
    }
}

async fn cli(temp: &TempDir, args: &[&str]) -> Result<String> {
    let output = tokio::time::timeout(
        Duration::from_secs(15),
        Command::new(env!("CARGO_BIN_EXE_exo"))
            .env_clear()
            .env("EXO_CONFIG_DIR", temp.path().join("config"))
            .env("TEST_RUNTIME_TOKEN", "usage-test-token")
            .current_dir(temp.path())
            .args(args)
            .kill_on_drop(true)
            .output(),
    )
    .await??;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(String::from_utf8(output.stdout)?)
}

#[actix_web::test]
async fn cli_reports_http_usage_across_restarts_and_paginated_history() -> Result<()> {
    let temp = TempDir::new()?;
    let config = BasicExoHarnessConfig {
        root: temp.path().join("server"),
        secret_backend: SecretBackendChoice::Static([7; 32]),
        sandbox_default: SandboxProvider::LocalProcess,
        sandbox_policy: None,
        sandbox_backends: vec![SandboxBackendRegistration::local_process()],
    };
    let state = Arc::new(BasicExoHarness::in_memory(config.clone()).await?);

    let pricing = cost::PricingTable::from_json_str(
        r#"{
        "metered-model": {"input_cost_per_token": 0.00001, "output_cost_per_token": 0.0001}
    }"#,
    )?;
    exoharness::vault::global_vault(state.as_ref())
        .await?
        .put_secret(exoharness::PutSecretRequest {
            name: "test-openai".into(),
            policy: Some(
                exoharness::CredentialDestination::origin("https://api.openai.com")
                    .unwrap()
                    .into(),
            ),
            secret: exoharness::Secret::Key {
                value: "usage-key".into(),
            },
        })
        .await?;
    let runtime = Arc::new(Runtime::new(
        LocalProvider::basic(
            state,
            Arc::new(MeteredModel),
            Arc::new(BasicToolRuntime),
            Arc::new(pricing),
        ),
        None,
    ));
    let agent = runtime
        .create_agent(CreateAgentRequest {
            credential: Some("test-openai".into()),
            base_url: None,
            slug: "usage-test".into(),
            name: None,
            harness: AgentHarnessKind::Basic,
            typescript: None,
            enable_agent_tool_creation: false,
            sandbox_image: None,
            sandbox_provider: SandboxProvider::LocalProcess,
            sandbox_scope: None,
            enable_networking: false,
            model: "metered-model".into(),
            max_output_tokens: None,
            max_tool_round_trips: None,
            braintrust: None,
        })
        .await?;
    let thread = runtime
        .open_managed_thread(&agent, None, Default::default())
        .await?
        .thread;
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let endpoint = format!("http://{}/exo", listener.local_addr()?);
    let server = server(
        listener,
        Arc::new(RuntimeHttpService::new(
            runtime.clone(),
            Some("usage-test-token"),
        )?),
    )?;
    let handle = server.handle();
    actix_web::rt::spawn(server);
    cli(
        &temp,
        &[
            "provider",
            "create",
            "oss",
            "--url",
            &endpoint,
            "--api-key-env",
            "TEST_RUNTIME_TOKEN",
        ],
    )
    .await?;
    let agent_id = agent.record().id.to_string();
    let thread_id = thread.record().id.to_string();
    let output = cli(
        &temp,
        &[
            "--provider",
            "oss",
            "agent",
            "run",
            "--agent",
            &agent_id,
            "--thread",
            &thread_id,
            "--prompt",
            "priced",
        ],
    )
    .await?;
    assert!(
        output.contains("1,100 [1,100 total] tok, $0.01 [$0.01 total]"),
        "{output}"
    );
    let codex_turn = thread.begin_turn(Default::default()).await?;
    let mut codex_events = (0..101)
        .map(|_| EventData::Messages {
            messages: vec![Message::Assistant {
                content: AssistantContent::String("Codex output".into()),
                id: None,
            }],
            response_id: None,
            usage: None,
        })
        .collect::<Vec<_>>();
    codex_events.push(EventData::Messages {
        messages: vec![],
        response_id: None,
        usage: Some(Box::new(UsageRecord {
            prompt_tokens: Some(10),
            completion_tokens: Some(2),
            cost_usd: Some(0.03),
            ..Default::default()
        })),
    });
    codex_turn.add_events(codex_events).await?;
    codex_turn.finish().await?;
    thread
        .add_events(AddEventsRequest {
            session_id: None,
            turn_id: None,
            data: (0..205)
                .map(|_| EventData::Messages {
                    messages: vec![],
                    response_id: None,
                    usage: Some(Box::new(UsageRecord {
                        prompt_tokens: Some(2),
                        completion_tokens: Some(1),
                        cost_usd: Some(0.002),
                        ..Default::default()
                    })),
                })
                .collect(),
        })
        .await?;
    for (input, expected) in [
        ("estimated", "1,100 [2,827 total] tok, $0.02 [$0.47 total]"),
        ("unpriced", "1,100 [3,927 total] tok, cost: unavailable"),
        ("missing", "tokens: unavailable, cost: unavailable"),
        ("priced", "1,100 tok, $0.01"),
        ("cost-only", "tokens: unavailable, $0.03"),
    ] {
        let output = cli(
            &temp,
            &[
                "--provider",
                "oss",
                "agent",
                "run",
                "--agent",
                &agent_id,
                "--thread",
                &thread_id,
                "--prompt",
                input,
            ],
        )
        .await?;
        assert!(output.contains(expected), "{input}: {output}");
    }
    runtime.shutdown().await?;
    handle.stop(false).await;
    Ok(())
}
