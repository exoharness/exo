use std::{collections::HashSet, ops::Bound};

use anyhow::{Context, Result, ensure};
use exo_managed_agents::http::protocol::{FrontendToolExecutionResult, FrontendToolResultBody};
use exoharness::{
    Event, EventData, EventId, EventKind, EventQuery, EventQueryDirection, ThreadHandle,
    ToolCallId, ToolRequest, ToolResult, TurnHandle, TurnRecord,
};
use futures::StreamExt;
use serde::{Deserialize, Serialize};

use crate::{AgentConfig, ToolDefinition, harness_executor::RecoverableTurn};

pub(crate) const FRONTEND_TOOL_RESPONSE: &str = "agent_runtime.frontend_tool_response";

#[derive(Debug, Serialize, Deserialize)]
struct Response {
    tool_call_id: ToolCallId,
    result: FrontendToolExecutionResult,
}

pub(crate) fn contains(config: &AgentConfig, name: &str) -> bool {
    config.frontend_tools.iter().any(|tool| tool.name == name)
}

pub(crate) fn definitions(config: &AgentConfig) -> Vec<ToolDefinition> {
    config
        .frontend_tools
        .iter()
        .map(|tool| ToolDefinition {
            strict: None,
            name: tool.name.clone(),
            description: tool.description.clone(),
            parameters: tool.parameters.clone(),
        })
        .collect()
}

pub(crate) fn validate(config: &AgentConfig, existing: &[ToolDefinition]) -> Result<()> {
    let mut names: HashSet<_> = existing.iter().map(|tool| tool.name.as_str()).collect();
    for tool in &config.frontend_tools {
        ensure!(
            !tool.name.trim().is_empty(),
            "client tool name must not be empty"
        );
        ensure!(
            names.insert(&tool.name),
            "duplicate tool name: {}",
            tool.name
        );
    }
    Ok(())
}

async fn events(thread: &dyn ThreadHandle, turn: &TurnRecord) -> Result<Vec<Event>> {
    let mut query = EventQuery {
        turn_id: Some(turn.id),
        session_id: Some(turn.session_id),
        direction: Some(EventQueryDirection::Asc),
        types: Some(vec![
            EventKind::custom(crate::harness_executor::RUNTIME_TURN_WORK),
            EventKind::custom(FRONTEND_TOOL_RESPONSE),
            EventKind::TOOL_REQUESTED,
            EventKind::TOOL_RESULT,
            EventKind::TURN_ENDED,
        ]),
        limit: Some(100),
        ..Default::default()
    };
    let mut events = Vec::new();
    loop {
        let page = thread.get_events(Some(query.clone())).await?;
        events.extend(page.events);
        match page.cursor {
            Some(cursor) => query.cursor = Some(cursor),
            None => return Ok(events),
        }
    }
}

fn response(event: &Event, tool_call_id: &str) -> Result<Option<Response>> {
    if let EventData::Custom {
        event_type,
        payload,
    } = &event.data
        && event_type == FRONTEND_TOOL_RESPONSE
    {
        let response: Response = serde_json::from_value(payload.clone())?;
        if response.tool_call_id == tool_call_id {
            return Ok(Some(response));
        }
    }
    Ok(None)
}

pub(crate) fn model_input(
    event: &Event,
) -> Result<Option<(ToolCallId, lingua::universal::UserContent)>> {
    if let EventData::Custom {
        event_type,
        payload,
    } = &event.data
        && event_type == FRONTEND_TOOL_RESPONSE
    {
        let response: Response = serde_json::from_value(payload.clone())?;
        if let FrontendToolExecutionResult::FrontendToolSuccess {
            model_input: Some(content),
            ..
        } = response.result
        {
            return Ok(Some((response.tool_call_id, content)));
        }
    }
    Ok(None)
}

pub(crate) async fn model_input_for_call(
    thread: &dyn ThreadHandle,
    turn: &TurnRecord,
    id: &str,
) -> Result<Option<lingua::universal::UserContent>> {
    for event in events(thread, turn).await? {
        if let Some((call, content)) = model_input(&event)?
            && call == id
        {
            return Ok(Some(content));
        }
    }
    Ok(None)
}

fn output(result: FrontendToolExecutionResult) -> Result<ToolResult> {
    #[derive(Serialize)]
    struct Failure {
        ok: bool,
        output: ToolResult,
        error: exo_managed_agents::http::protocol::FrontendToolExecutionError,
    }
    match result {
        FrontendToolExecutionResult::FrontendToolSuccess { output, .. } => Ok(output),
        FrontendToolExecutionResult::FrontendToolError { output, error } => {
            Ok(serde_json::to_value(Failure {
                ok: false,
                output,
                error,
            })?)
        }
    }
}

pub(crate) async fn execute(
    thread: &dyn ThreadHandle,
    turn: &dyn TurnHandle,
    tool_call_id: &str,
    request: &ToolRequest,
) -> Result<ToolResult> {
    let saved = events(thread, turn.record()).await?;
    ensure!(
        saved.iter().any(|event| matches!(&event.data,
            EventData::ToolRequested { tool_call_id: id, request: saved, .. }
                if id == tool_call_id && saved == request
        )),
        "client tool call must be persisted before execution"
    );
    for event in &saved {
        if let Some(response) = response(event, tool_call_id)? {
            return output(response.result);
        }
        ensure!(
            !matches!(event.data, EventData::TurnEnded),
            "turn has ended"
        );
    }
    let after = saved.last().context("client tool call has no event")?.id;
    let mut stream = thread.watch_events(Bound::Excluded(after)).await?;
    while let Some(event) = stream.next().await {
        let event = event?;
        if event.turn_id != Some(turn.record().id)
            || event.session_id != Some(turn.record().session_id)
        {
            continue;
        }
        if let Some(response) = response(&event, tool_call_id)? {
            return output(response.result);
        }
        ensure!(
            !matches!(event.data, EventData::TurnEnded),
            "turn has ended"
        );
    }
    anyhow::bail!("event stream ended while waiting for client tool result")
}

/// The caller holds the provider's response lock across validation and append.
pub(crate) async fn respond(
    thread: &dyn ThreadHandle,
    turn: TurnRecord,
    body: &FrontendToolResultBody,
) -> Result<EventId> {
    let saved = events(thread, &turn).await?;
    let mut work = None;
    let mut request = None;
    let mut completed = false;
    for event in &saved {
        if let Some(response) = response(event, &body.tool_call_id)? {
            ensure!(
                serde_json::to_value(response.result)? == serde_json::to_value(&body.result)?,
                "client tool already has a different result"
            );
            return Ok(event.id);
        }
        match &event.data {
            EventData::Custom {
                event_type,
                payload,
            } if event_type == crate::harness_executor::RUNTIME_TURN_WORK => {
                work = Some(serde_json::from_value::<RecoverableTurn>(payload.clone())?);
            }
            EventData::ToolRequested {
                tool_call_id,
                request: pending,
                ..
            } if tool_call_id == &body.tool_call_id => request = Some(pending),
            EventData::ToolResult { tool_call_id, .. } if tool_call_id == &body.tool_call_id => {
                completed = true
            }
            EventData::TurnEnded => completed = true,
            _ => {}
        }
    }
    ensure!(!completed, "client tool call is no longer pending");
    let request = request.context("tool call is not pending for this session and turn")?;
    ensure!(
        contains(
            &work
                .context("turn has no saved configuration")?
                .agent_config,
            &request.function_name
        ),
        "tool call is not a client tool"
    );
    let data = vec![EventData::Custom {
        event_type: FRONTEND_TOOL_RESPONSE.into(),
        payload: serde_json::to_value(Response {
            tool_call_id: body.tool_call_id.clone(),
            result: body.result.clone(),
        })?,
    }];
    Ok(thread
        .turn_handle(turn)
        .await?
        .add_events(data)
        .await?
        .latest_event_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        BasicExecutor, BasicToolRuntime, ConversationConfig, ModelClient, ModelRequest,
        ModelResponse, ModelResponseStream, SendRequest,
        harness_executor::{ExecutorStreamMode, HarnessExecutor},
    };
    use exo_managed_agents::{
        AgentDefinition,
        http::protocol::{
            FrontendToolDefinition, FrontendToolExecutionError, FrontendToolExecutionErrorType,
        },
    };
    use exoharness::{
        AgentHandle, BasicExoHarness, BeginTurnRequest, ExoHarness, NewAgentRequest,
        SandboxProvider,
    };
    use std::sync::{Arc, Mutex};

    struct Fixture {
        state: Arc<dyn ExoHarness>,
        agent: Arc<dyn AgentHandle>,
        thread: Arc<dyn ThreadHandle>,
        turn: Arc<dyn TurnHandle>,
        config: AgentConfig,
        request: ToolRequest,
    }

    impl Fixture {
        async fn new() -> Result<Self> {
            let state: Arc<dyn ExoHarness> = Arc::new(
                BasicExoHarness::in_memory(crate::test_support::local_test_config(
                    "unused-client-tools",
                ))
                .await?,
            );
            crate::test_support::create_test_credential(state.as_ref()).await;
            let agent = state
                .new_agent(NewAgentRequest {
                    name: "Client tools".into(),
                    slug: "client-tools".into(),
                    vaults: vec![],
                })
                .await?;
            let thread = agent.new_thread(Default::default()).await?;
            let mut config = crate::managed_agents::agent_config(&AgentDefinition::parse(
                "---\nharness: basic\nmodel:\n  name: gpt-5.4\n  credential: test-openai\n---\nTest.".into()
            )?, SandboxProvider::LocalProcess, None, None)?;
            config.frontend_tools.push(FrontendToolDefinition {
                name: "lookup".into(),
                description: "Read from the client".into(),
                parameters: serde_json::json!({"type": "object", "properties": {}}),
                defer_loading: false,
            });
            let work = RecoverableTurn {
                agent_config: config.clone(),
                thread_config: ConversationConfig::default(),
                request: SendRequest {
                    input: vec![],
                    session_id: None,
                },
            };
            let request = ToolRequest {
                namespace: None,
                function_name: "lookup".into(),
                arguments: Default::default(),
            };
            let turn = thread
                .begin_turn(BeginTurnRequest {
                    initial_events: vec![
                        work.event()?,
                        EventData::Custom {
                            event_type: crate::basic::BASIC_TOOL_ROUND.into(),
                            payload: serde_json::to_value(crate::basic::BasicToolRound {
                                round: 0,
                            })?,
                        },
                        EventData::ToolRequested {
                            tool_call_id: "call-1".into(),
                            response_id: None,
                            request: request.clone(),
                        },
                    ],
                    ..Default::default()
                })
                .await?;
            Ok(Self {
                state,
                agent,
                thread,
                turn,
                config,
                request,
            })
        }

        fn result(&self) -> FrontendToolResultBody {
            FrontendToolResultBody {
                session_id: self.turn.record().session_id,
                tool_call_id: "call-1".into(),
                result: FrontendToolExecutionResult::FrontendToolSuccess {
                    output: serde_json::json!({"answer": 42}),
                    model_input: None,
                },
            }
        }
    }

    #[tokio::test]
    async fn waits_for_client_result_and_reuses_it_on_recovery() -> Result<()> {
        let f = Fixture::new().await?;
        let body = f.result();
        let (result, saved) = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            tokio::join!(
                execute(f.thread.as_ref(), f.turn.as_ref(), "call-1", &f.request),
                respond(f.thread.as_ref(), f.turn.record().clone(), &body)
            )
        })
        .await?;
        assert_eq!(result?, serde_json::json!({"answer": 42}));
        let id = saved?;
        assert_eq!(
            respond(f.thread.as_ref(), f.turn.record().clone(), &body).await?,
            id
        );
        assert_eq!(
            execute(f.thread.as_ref(), f.turn.as_ref(), "call-1", &f.request).await?,
            serde_json::json!({"answer": 42})
        );
        let saved = events(f.thread.as_ref(), f.turn.record()).await?;
        assert_eq!(saved.iter().filter(|event| matches!(&event.data, EventData::Custom { event_type, .. } if event_type == FRONTEND_TOOL_RESPONSE)).count(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn rejects_wrong_session_unknown_call_and_conflicting_retry() -> Result<()> {
        let f = Fixture::new().await?;
        let mut body = f.result();
        let mut wrong_session = f.turn.record().clone();
        wrong_session.session_id = exoharness::Uuid7::now();
        assert!(
            respond(f.thread.as_ref(), wrong_session, &body)
                .await
                .is_err()
        );
        body.tool_call_id = "not-a-call".into();
        assert!(
            respond(f.thread.as_ref(), f.turn.record().clone(), &body)
                .await
                .is_err()
        );
        body = f.result();
        respond(f.thread.as_ref(), f.turn.record().clone(), &body).await?;
        body.result = FrontendToolExecutionResult::FrontendToolSuccess {
            output: serde_json::json!(43),
            model_input: None,
        };
        assert!(
            respond(f.thread.as_ref(), f.turn.record().clone(), &body)
                .await
                .is_err()
        );
        Ok(())
    }

    #[tokio::test]
    async fn client_errors_keep_their_kind_and_output() -> Result<()> {
        let f = Fixture::new().await?;
        let mut body = f.result();
        body.result = FrontendToolExecutionResult::FrontendToolError {
            output: serde_json::json!({"partial": true}),
            error: FrontendToolExecutionError {
                r#type: FrontendToolExecutionErrorType::UserRejected,
                message: "Declined".into(),
            },
        };
        respond(f.thread.as_ref(), f.turn.record().clone(), &body).await?;
        assert_eq!(
            execute(f.thread.as_ref(), f.turn.as_ref(), "call-1", &f.request).await?,
            serde_json::json!({
                "ok": false, "output": {"partial": true}, "error": {"type": "user_rejected", "message": "Declined"}
            })
        );
        Ok(())
    }

    #[derive(Default)]
    struct RecordingModel(Mutex<Vec<ModelRequest>>);

    struct ResponseStream(ModelResponse);

    #[async_trait::async_trait]
    impl ModelResponseStream for ResponseStream {
        async fn next_chunk(&mut self) -> Result<Option<lingua::UniversalStreamChunk>> {
            Ok(None)
        }
        async fn finish(self: Box<Self>) -> Result<ModelResponse> {
            Ok(self.0)
        }
    }

    #[async_trait::async_trait]
    impl ModelClient for RecordingModel {
        async fn complete(&self, request: ModelRequest) -> Result<ModelResponse> {
            let needs_lookup = request.tools.iter().any(|tool| tool.name == "lookup")
                && !request
                    .messages
                    .iter()
                    .any(|message| matches!(message, lingua::Message::Tool { .. }));
            self.0.lock().unwrap().push(request);
            Ok(ModelResponse {
                response_id: None,
                messages: if needs_lookup {
                    vec![]
                } else {
                    vec![crate::harness_helpers::assistant_message("Done")]
                },
                tool_calls: if needs_lookup {
                    vec![crate::PendingToolCall {
                        tool_call_id: "call-1".into(),
                        request: ToolRequest {
                            namespace: None,
                            function_name: "lookup".into(),
                            arguments: Default::default(),
                        },
                    }]
                } else {
                    vec![]
                },
                usage: None,
                model: None,
                provider_cost_usd: None,
                ttft: None,
                duration: None,
            })
        }
        async fn complete_stream(
            &self,
            request: ModelRequest,
        ) -> Result<Box<dyn ModelResponseStream>> {
            Ok(Box::new(ResponseStream(self.complete(request).await?)))
        }
    }

    #[tokio::test]
    async fn basic_resume_consumes_saved_client_result_before_calling_model() -> Result<()> {
        let f = Fixture::new().await?;
        let mut body = f.result();
        body.result = FrontendToolExecutionResult::FrontendToolSuccess {
            output: serde_json::json!({"answer": 42}),
            model_input: Some(lingua::universal::UserContent::String(
                "Additional client context".into(),
            )),
        };
        respond(f.thread.as_ref(), f.turn.record().clone(), &body).await?;
        let model = Arc::new(RecordingModel::default());
        let executor = BasicExecutor::new(model.clone(), Arc::new(BasicToolRuntime));
        executor
            .resume_turn(
                f.agent.as_ref(),
                f.thread.clone(),
                f.turn.clone(),
                &f.config,
                &ConversationConfig::default(),
                &SendRequest {
                    input: vec![],
                    session_id: None,
                },
                ExecutorStreamMode::Disabled,
                None,
            )
            .await?;
        {
            let requests = model.0.lock().unwrap();
            assert_eq!(requests.len(), 1);
            assert!(requests[0].tools.iter().any(|tool| tool.name == "lookup"));
            let tool_index = requests[0]
                .messages
                .iter()
                .position(|message| matches!(message, lingua::Message::Tool { .. }))
                .unwrap();
            assert!(
                matches!(&requests[0].messages[tool_index + 1], lingua::Message::User { content: lingua::universal::UserContent::String(text) } if text == "Additional client context")
            );
        }
        let saved = events(f.thread.as_ref(), f.turn.record()).await?;
        assert_eq!(
            saved
                .iter()
                .filter(|event| matches!(event.data, EventData::ToolRequested { .. }))
                .count(),
            1
        );
        assert_eq!(saved.iter().filter(|event| matches!(&event.data, EventData::ToolResult { tool_call_id, result } if tool_call_id == "call-1" && *result == serde_json::json!({"answer": 42}))).count(), 1);
        Ok(())
    }

    #[actix_web::test]
    async fn http_client_tool_result_continues_the_waiting_turn() -> Result<()> {
        use actix_web::{App, test, web};
        use exo_managed_agents::http::protocol::{
            EventResult, OneOrMany, SubmitTurnBody, SubmitTurnResult,
        };
        let f = Fixture::new().await?;
        let mut saved_config = f.config.clone();
        saved_config.frontend_tools.clear();
        crate::harness_config::store_agent_config(f.agent.as_ref(), &saved_config).await?;
        let thread = f.agent.new_thread(Default::default()).await?;
        let model = Arc::new(RecordingModel::default());
        let runtime = Arc::new(crate::Runtime::new(
            crate::LocalProvider::basic(
                f.state.clone(),
                model.clone(),
                Arc::new(BasicToolRuntime),
                Arc::new(cost::PricingTable::empty()),
            ),
            None,
        ));
        let service = Arc::new(crate::http_service::RuntimeHttpService::new(
            runtime.clone(),
            None,
        )?);
        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(service))
                .configure(crate::http_service::configure),
        )
        .await;
        let path = format!(
            "/exo/agent/{}/thread/{}/turn",
            f.agent.record().id,
            thread.record().id
        );
        let mut stream = thread.watch_events(Bound::Unbounded).await?;
        let submitted: SubmitTurnResult = test::call_and_read_body_json(
            &app,
            test::TestRequest::post()
                .uri(&path)
                .set_json(SubmitTurnBody::<()> {
                    input: Some(OneOrMany::One(crate::harness_helpers::user_message(
                        "Look up the answer.",
                    ))),
                    frontend_tools: Some(OneOrMany::Many(f.config.frontend_tools)),
                    ..Default::default()
                })
                .to_request(),
        )
        .await;
        let call = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while let Some(event) = stream.next().await {
                match event?.data {
                    EventData::ToolRequested { tool_call_id, .. } => {
                        return Ok::<_, anyhow::Error>(tool_call_id);
                    }
                    EventData::Error { message, .. } => anyhow::bail!("turn failed: {message}"),
                    _ => {}
                }
            }
            anyhow::bail!("no client call")
        })
        .await
        .context("waiting for client tool request")??;
        let body = FrontendToolResultBody {
            session_id: submitted.turn.session_id,
            tool_call_id: call,
            result: FrontendToolExecutionResult::FrontendToolSuccess {
                output: serde_json::json!({"answer": 42}),
                model_input: None,
            },
        };
        let _: EventResult = test::call_and_read_body_json(
            &app,
            test::TestRequest::post()
                .uri(&format!(
                    "{path}/{}/frontend-tool-result",
                    submitted.turn.id
                ))
                .set_json(body)
                .to_request(),
        )
        .await;
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while let Some(event) = stream.next().await {
                match event?.data {
                    EventData::TurnEnded => return Ok::<_, anyhow::Error>(()),
                    EventData::Error { message, .. } => anyhow::bail!("turn failed: {message}"),
                    _ => {}
                }
            }
            anyhow::bail!("turn did not finish")
        })
        .await
        .context("waiting for turn completion after client tool result")??;
        assert_eq!(model.0.lock().unwrap().len(), 2);
        runtime.shutdown().await?;
        Ok(())
    }
}
