use std::collections::VecDeque;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

use crate::{
    ModelClient, ModelRequest, ModelResponse, ModelResponseStream, PendingToolCall, SendRequest,
    ToolRuntime,
};
use anyhow::anyhow;
use async_trait::async_trait;
use exoharness::{
    AddEventsRequest, AgentHandle, BasicExoHarness, BeginTurnRequest, ConversationHandle,
    EventData, EventKind, EventQuery, EventQueryDirection, ExoHarness, FileSystemMount,
    FileSystemMountMode, PutSecretRequest, ResourceScope, Result, SandboxAttachment,
    SandboxProvider, Secret, ToolRequest, ToolResult, TurnHandle, Uuid7,
    access::{AccessPolicy, Caller},
    vault::VaultRecord,
};
use futures::StreamExt;
use lingua::universal::{AssistantContent, UserContent};
use lingua::{Message, UniversalStreamChunk, UniversalUsage};
use serde_json::{Map, Value};
use tempfile::TempDir;

use crate::test_support::{create_test_credential, local_test_config};
use crate::{
    BasicToolRuntime, ConversationModelConfig, CreateAgentRequest, CreateConversationRequest,
    LocalProvider, Runtime,
    harness_executor::{ExecutorStreamMode, HarnessExecutor, RecoveryRuntimeResolver},
    harness_executor::{RUNTIME_TURN_COMPLETED, RecoverableTurn},
    harness_tool::ensure_shell_sandbox,
    http_service::{RuntimeHttpService, server},
};

struct RecoveryPolicy(Uuid7);

async fn wait_for_active_turn(
    runtime: &Runtime,
    thread: &dyn ConversationHandle,
    turn: exoharness::TurnId,
) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(3), async {
        while !runtime.is_turn_active(thread, turn).await? {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await?
}

#[async_trait]
impl AccessPolicy for RecoveryPolicy {
    async fn check(&self, _: &str, _: ResourceScope) -> Result<()> {
        Ok(())
    }
    async fn check_operator(&self, _: &str) -> Result<()> {
        Ok(())
    }
    async fn created(&self, _: &str, _: ResourceScope) -> Result<()> {
        Ok(())
    }
    async fn vault(&self, _: &str, record: &VaultRecord, _: bool) -> Result<Option<VaultRecord>> {
        Ok(Some(record.clone()))
    }
    async fn vault_created(&self, _: &str, _: Uuid7, _: &str) -> Result<()> {
        Ok(())
    }
    async fn default_vault(&self, _: &str) -> Result<Uuid7> {
        Ok(self.0)
    }
}

#[derive(Clone, Default)]
struct CallerRecordingExecutor(Arc<Mutex<Vec<Option<String>>>>);

#[derive(Default)]
struct CountingToolRuntime(AtomicUsize);

#[async_trait]
impl ToolRuntime for CountingToolRuntime {
    fn permission_policy(
        &self,
        policies: &exo_managed_agents::permissions::PermissionPolicies,
        name: &str,
    ) -> exo_managed_agents::permissions::PermissionPolicy {
        if name == "write_file" {
            exo_managed_agents::permissions::PermissionPolicy::AlwaysAsk {}
        } else {
            policies.for_tool(name)
        }
    }

    async fn execute(
        &self,
        _: &dyn AgentHandle,
        _: &dyn ConversationHandle,
        _: Option<&dyn TurnHandle>,
        _: &crate::AgentConfig,
        _: &crate::ConversationConfig,
        _: &ToolRequest,
    ) -> Result<ToolResult> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(serde_json::json!({"ok": true}))
    }
}

#[async_trait]
impl HarnessExecutor for CallerRecordingExecutor {
    fn name(&self) -> &'static str {
        "caller-recording"
    }

    async fn execute_turn(
        &self,
        _: &dyn AgentHandle,
        thread: Arc<dyn ConversationHandle>,
        _: Arc<dyn TurnHandle>,
        _: &crate::AgentConfig,
        _: &crate::ConversationConfig,
        _: &SendRequest,
        _: ExecutorStreamMode<'_>,
        _: Option<&dyn crate::execution_tracing::TurnExecutionTrace>,
    ) -> Result<()> {
        self.0
            .lock()
            .expect("caller record poisoned")
            .push(thread.caller().map(|caller| caller.principal.clone()));
        Ok(())
    }

    async fn resume_turn(
        &self,
        agent: &dyn AgentHandle,
        thread: Arc<dyn ConversationHandle>,
        turn: Arc<dyn TurnHandle>,
        agent_config: &crate::AgentConfig,
        conversation_config: &crate::ConversationConfig,
        request: &SendRequest,
        stream_mode: ExecutorStreamMode<'_>,
        turn_trace: Option<&dyn crate::execution_tracing::TurnExecutionTrace>,
    ) -> Result<()> {
        self.execute_turn(
            agent,
            thread,
            turn,
            agent_config,
            conversation_config,
            request,
            stream_mode,
            turn_trace,
        )
        .await
    }
}

#[tokio::test]
async fn recovery_leaves_threads_owned_by_another_process_untouched() -> Result<()> {
    let temp = TempDir::new()?;
    let config = local_test_config(temp.path());
    let inline = BasicExoHarness::new(config.clone())
        .await?
        .with_local_sessions(temp.path().to_owned());
    let agent = exoharness::test_support::new_test_agent(&inline, "owned-recovery").await?;
    let thread = agent.new_thread(Default::default()).await?;
    thread
        .create_sandbox(exoharness::test_support::sandbox_request())
        .await?;
    let work = RecoverableTurn {
        streaming: false,
        agent_config: serde_json::from_str(
            r#"{"model":"unused","instructions":[],"sandbox":{"provider":"local_process"}}"#,
        )?,
        thread_config: Default::default(),
        request: SendRequest {
            session_id: None,
            input: Vec::new(),
        },
    };
    thread
        .begin_turn(BeginTurnRequest {
            initial_events: vec![
                work.event()?,
                EventData::Custom {
                    event_type: RUNTIME_TURN_COMPLETED.into(),
                    payload: Value::Null,
                },
            ],
            ..Default::default()
        })
        .await?;
    // Recovery would normally finalize this completed-but-unfinished turn.
    // Its owner must be allowed to finish it without another process writing.
    let before = serde_json::to_vec(&thread.get_events(None).await?.events)?;
    let server = Runtime::new(
        LocalProvider::basic(
            Arc::new(
                BasicExoHarness::new(config)
                    .await?
                    .with_local_sessions(temp.path().to_owned()),
            ),
            Arc::new(FakeModelClient::default()),
            Arc::new(BasicToolRuntime),
            Arc::new(cost::PricingTable::empty()),
        ),
        None,
    );
    server.recover_unfinished_turns().await?;
    server.shutdown().await?;
    assert_eq!(
        serde_json::to_vec(&thread.get_events(None).await?.events)?,
        before
    );
    assert!(thread.list_sandboxes().await?[0].running);
    inline.release_local_sessions().await
}

#[tokio::test]
async fn service_restart_resumes_turn_after_completed_tool_result() -> Result<()> {
    let tempdir = TempDir::new()?;
    let root = tempdir.path().join("exoharness");
    let state: Arc<dyn ExoHarness> =
        Arc::new(BasicExoHarness::new(local_test_config(&root)).await?);
    let runtime = Runtime::new(
        LocalProvider::basic(
            Arc::clone(&state),
            Arc::new(FakeModelClient::default()),
            Arc::new(BasicToolRuntime),
            Arc::new(cost::PricingTable::empty()),
        ),
        None,
    );
    create_test_credential(state.as_ref()).await;
    let agent = runtime
        .create_agent(CreateAgentRequest {
            ..crate::test_support::agent_request("durable-turn", crate::AgentHarnessKind::Basic)
        })
        .await?;
    let thread = runtime
        .create_conversation(agent.as_ref(), CreateConversationRequest::default())
        .await?;
    let agent_id = agent.record().id;
    let thread_id = thread.record().id;
    let request = SendRequest {
        input: vec![user_message("continue after the tool")],
        session_id: None,
    };
    let mut agent_config = runtime.get_agent_config(agent.as_ref()).await?;
    agent_config.model = "recovery-model".into();
    let work = RecoverableTurn {
        streaming: false,
        agent_config,
        thread_config: runtime.get_conversation_config(thread.as_ref()).await?,
        request: request.clone(),
    };
    let turn = thread
        .begin_turn(BeginTurnRequest {
            turn: None,
            session_id: None,
            input: request.input,
            initial_events: vec![work.event()?],
        })
        .await?;
    let turn_id = turn.record().id;
    turn.add_events(vec![
        EventData::Custom {
            event_type: crate::basic::BASIC_TOOL_ROUND.into(),
            payload: serde_json::to_value(crate::basic::BasicToolRound { round: 0 })?,
        },
        EventData::ToolRequested {
            tool_call_id: "call-1".into(),
            response_id: None,
            request: ToolRequest {
                namespace: None,
                function_name: "lookup".into(),
                arguments: Map::new(),
            },
        },
        EventData::ToolResult {
            tool_call_id: "call-1".into(),
            result: serde_json::json!({"answer": "saved result"}),
        },
    ])
    .await?;
    let completed_thread = runtime
        .create_conversation(
            agent.as_ref(),
            CreateConversationRequest {
                slug: Some("completed".into()),
                ..Default::default()
            },
        )
        .await?;
    let completed_thread_id = completed_thread.record().id;
    let completed_work = RecoverableTurn {
        streaming: false,
        request: SendRequest {
            input: vec![user_message("already answered")],
            session_id: None,
        },
        ..work.clone()
    };
    let completed_turn = completed_thread
        .begin_turn(BeginTurnRequest {
            turn: None,
            session_id: None,
            input: completed_work.request.input.clone(),
            initial_events: vec![completed_work.event()?],
        })
        .await?;
    let completed_turn_id = completed_turn.record().id;
    completed_turn
        .add_events(vec![
            EventData::Messages {
                messages: vec![assistant_message("saved answer")],
                response_id: None,
                usage: None,
            },
            EventData::Custom {
                event_type: RUNTIME_TURN_COMPLETED.into(),
                payload: Value::Null,
            },
        ])
        .await?;
    let failed_thread = runtime
        .create_conversation(
            agent.as_ref(),
            CreateConversationRequest {
                slug: Some("failed".into()),
                ..Default::default()
            },
        )
        .await?;
    let failed_thread_id = failed_thread.record().id;
    let failed_turn = failed_thread
        .begin_turn(BeginTurnRequest {
            turn: None,
            session_id: None,
            input: vec![user_message("already failed")],
            initial_events: vec![work.event()?],
        })
        .await?;
    let failed_turn_id = failed_turn.record().id;
    failed_turn
        .add_events(vec![EventData::Error {
            message: "execution already failed".into(),
            metadata: None,
        }])
        .await?;
    let invalid_thread = runtime
        .create_conversation(
            agent.as_ref(),
            CreateConversationRequest {
                slug: Some("invalid-work".into()),
                ..Default::default()
            },
        )
        .await?;
    let invalid_thread_id = invalid_thread.record().id;
    let invalid_turn = invalid_thread
        .begin_turn(BeginTurnRequest {
            turn: None,
            session_id: None,
            input: vec![user_message("invalid work")],
            initial_events: vec![EventData::Custom {
                event_type: crate::harness_executor::RUNTIME_TURN_WORK.into(),
                payload: Value::Null,
            }],
        })
        .await?;
    let invalid_turn_id = invalid_turn.record().id;
    drop(invalid_turn);
    drop(invalid_thread);
    drop(failed_turn);
    drop(failed_thread);
    drop(completed_turn);
    drop(completed_thread);
    drop(turn);
    drop(thread);
    drop(agent);
    drop(runtime);
    drop(state);

    let state: Arc<dyn ExoHarness> =
        Arc::new(BasicExoHarness::new(local_test_config(&root)).await?);
    let model = Arc::new(FakeModelClient::new(vec![ModelResponse {
        provider_cost_usd: None,
        response_id: None,
        messages: vec![assistant_message("resumed")],
        tool_calls: Vec::new(),
        usage: None,
        model: None,
        ttft: None,
        duration: None,
    }]));
    let runtime = Arc::new(Runtime::new(
        LocalProvider::basic(
            state,
            Arc::clone(&model),
            Arc::new(BasicToolRuntime),
            Arc::new(cost::PricingTable::empty()),
        ),
        None,
    ));
    runtime.recover_unfinished_turns().await?;
    let agent = runtime
        .exoharness_handle()
        .get_agent(&agent_id)
        .await?
        .expect("agent should survive restart");
    let thread = agent
        .get_thread(&thread_id)
        .await?
        .expect("thread should survive restart");
    let completed_thread = agent
        .get_thread(&completed_thread_id)
        .await?
        .expect("completed thread should survive restart");
    let invalid_thread = agent
        .get_thread(&invalid_thread_id)
        .await?
        .expect("invalid thread should survive restart");
    let failed_thread = agent
        .get_thread(&failed_thread_id)
        .await?
        .expect("failed thread should survive restart");
    let events = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let events = thread
                .get_events(Some(EventQuery {
                    turn_id: Some(turn_id),
                    ..Default::default()
                }))
                .await?
                .events;
            if events
                .iter()
                .any(|event| matches!(event.data, EventData::TurnEnded))
            {
                return Ok::<_, anyhow::Error>(events);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await??;
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let events = completed_thread
                .get_events(Some(EventQuery {
                    turn_id: Some(completed_turn_id),
                    ..Default::default()
                }))
                .await?
                .events;
            if events
                .iter()
                .any(|event| matches!(event.data, EventData::TurnEnded))
            {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await??;
    assert_eq!(model.requests().len(), 1);
    assert_eq!(model.requests()[0].model, "recovery-model");
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event.data, EventData::TurnStarted { .. }))
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event.data, EventData::ToolRequested { .. }))
            .count(),
        1
    );
    assert!(
        model.requests()[0]
            .messages
            .iter()
            .any(|message| { matches!(message, Message::Tool { .. }) })
    );
    assert!(serde_json::to_string(&model.requests()[0].messages)?.contains("saved result"));
    let invalid_events = invalid_thread
        .get_events(Some(EventQuery {
            turn_id: Some(invalid_turn_id),
            ..Default::default()
        }))
        .await?
        .events;
    assert!(
        invalid_events
            .iter()
            .any(|event| matches!(event.data, EventData::Error { .. }))
    );
    assert!(
        invalid_events
            .iter()
            .any(|event| matches!(event.data, EventData::TurnEnded))
    );
    let failed_events = failed_thread
        .get_events(Some(EventQuery {
            turn_id: Some(failed_turn_id),
            ..Default::default()
        }))
        .await?
        .events;
    assert!(
        failed_events
            .iter()
            .any(|event| matches!(event.data, EventData::TurnEnded))
    );
    assert_eq!(model.requests().len(), 1);
    runtime.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn service_restart_rejects_unresolved_tool_call() -> Result<()> {
    let tempdir = TempDir::new()?;
    let root = tempdir.path().join("exoharness");
    let state: Arc<dyn ExoHarness> =
        Arc::new(BasicExoHarness::new(local_test_config(&root)).await?);
    let runtime = Runtime::new(
        LocalProvider::basic(
            Arc::clone(&state),
            Arc::new(FakeModelClient::default()),
            Arc::new(BasicToolRuntime),
            Arc::new(cost::PricingTable::empty()),
        ),
        None,
    );
    create_test_credential(state.as_ref()).await;
    let agent = runtime
        .create_agent(CreateAgentRequest {
            ..crate::test_support::agent_request("unresolved-tool", crate::AgentHarnessKind::Basic)
        })
        .await?;
    let thread = runtime
        .create_conversation(agent.as_ref(), CreateConversationRequest::default())
        .await?;
    let agent_id = agent.record().id;
    let thread_id = thread.record().id;
    let request = SendRequest {
        input: vec![user_message("run the tool")],
        session_id: None,
    };
    let work = RecoverableTurn {
        streaming: false,
        agent_config: runtime.get_agent_config(agent.as_ref()).await?,
        thread_config: runtime.get_conversation_config(thread.as_ref()).await?,
        request: request.clone(),
    };
    let turn = thread
        .begin_turn(BeginTurnRequest {
            turn: None,
            session_id: None,
            input: request.input,
            initial_events: vec![work.event()?],
        })
        .await?;
    let turn_id = turn.record().id;
    turn.add_events(vec![EventData::ToolRequested {
        tool_call_id: "call-1".into(),
        response_id: None,
        request: ToolRequest {
            namespace: None,
            function_name: "side_effect".into(),
            arguments: Map::new(),
        },
    }])
    .await?;
    let answered_thread = runtime
        .create_conversation(
            agent.as_ref(),
            CreateConversationRequest {
                slug: Some("answered-approval".into()),
                ..Default::default()
            },
        )
        .await?;
    let answered_thread_id = answered_thread.record().id;
    let answered_turn = answered_thread
        .begin_turn(BeginTurnRequest {
            turn: None,
            session_id: None,
            input: vec![user_message("approval was answered before crash")],
            initial_events: vec![work.event()?],
        })
        .await?;
    let answered_turn_id = answered_turn.record().id;
    let answered_request = ToolRequest {
        namespace: None,
        function_name: "side_effect".into(),
        arguments: Map::new(),
    };
    answered_turn
        .add_events(vec![
            EventData::Custom {
                event_type: crate::basic::BASIC_TOOL_ROUND.into(),
                payload: serde_json::to_value(crate::basic::BasicToolRound { round: 0 })?,
            },
            EventData::ToolRequested {
                tool_call_id: "answered-call".into(),
                response_id: None,
                request: answered_request.clone(),
            },
            EventData::Custom {
                event_type: crate::permissions::APPROVAL_REQUESTED.into(),
                payload: serde_json::to_value(crate::permissions::ApprovalRequest {
                    approval_id: "answered-approval".into(),
                    tool_call_id: Some("answered-call".into()),
                    round: Some(0),
                    request: answered_request,
                })?,
            },
        ])
        .await?;
    crate::permissions::respond(
        answered_thread.as_ref(),
        answered_turn.record().clone(),
        &exo_managed_agents::http::protocol::ApprovalResponseBody {
            session_id: answered_turn.record().session_id,
            approval_id: "answered-approval".into(),
            approved: true,
            allow_for_tool: false,
        },
    )
    .await?;
    drop(answered_turn);
    drop(answered_thread);
    drop(turn);
    drop(thread);
    drop(agent);
    drop(runtime);
    drop(state);

    let state: Arc<dyn ExoHarness> =
        Arc::new(BasicExoHarness::new(local_test_config(&root)).await?);
    let model = Arc::new(FakeModelClient::default());
    let runtime = Runtime::new(
        LocalProvider::basic(
            Arc::clone(&state),
            Arc::clone(&model),
            Arc::new(BasicToolRuntime),
            Arc::new(cost::PricingTable::empty()),
        ),
        None,
    );
    runtime.recover_unfinished_turns().await?;
    let agent = state.get_agent(&agent_id).await?.unwrap();
    let thread = agent.get_thread(&thread_id).await?.unwrap();
    let answered_thread = agent.get_thread(&answered_thread_id).await?.unwrap();
    let events = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let events = thread
                .get_events(Some(EventQuery {
                    turn_id: Some(turn_id),
                    ..Default::default()
                }))
                .await?
                .events;
            if events
                .iter()
                .any(|event| matches!(event.data, EventData::TurnEnded))
            {
                return Ok::<_, anyhow::Error>(events);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await??;
    assert!(events.iter().any(|event| matches!(
        &event.data,
        EventData::Error { message, .. }
            if message.contains("cannot safely resume unresolved tool call `call-1` (`side_effect`)")
    )));
    assert!(model.requests().is_empty());
    let answered_events = answered_thread
        .get_events(Some(EventQuery {
            turn_id: Some(answered_turn_id),
            ..Default::default()
        }))
        .await?
        .events;
    assert!(answered_events.iter().any(|event| matches!(
        &event.data,
        EventData::Error { message, .. }
            if message.contains("cannot safely resume unresolved tool call `answered-call`")
    )));
    assert!(
        answered_events
            .iter()
            .any(|event| matches!(event.data, EventData::TurnEnded))
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event.data, EventData::ToolResult { .. }))
    );
    runtime.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn service_restart_resumes_pending_tool_approval() -> Result<()> {
    let tempdir = TempDir::new()?;
    let root = tempdir.path().join("exoharness");
    let state: Arc<dyn ExoHarness> =
        Arc::new(BasicExoHarness::new(local_test_config(&root)).await?);
    let runtime = Runtime::new(
        LocalProvider::basic(
            Arc::clone(&state),
            Arc::new(FakeModelClient::default()),
            Arc::new(CountingToolRuntime::default()),
            Arc::new(cost::PricingTable::empty()),
        ),
        None,
    );
    create_test_credential(state.as_ref()).await;
    let agent = runtime
        .create_agent(CreateAgentRequest {
            ..crate::test_support::agent_request("pending-approval", crate::AgentHarnessKind::Basic)
        })
        .await?;
    let thread = runtime
        .create_conversation(agent.as_ref(), CreateConversationRequest::default())
        .await?;
    let agent_id = agent.record().id;
    let thread_id = thread.record().id;
    let request = SendRequest {
        input: vec![user_message("run approved tool")],
        session_id: None,
    };
    let mut thread_config = runtime.get_conversation_config(thread.as_ref()).await?;
    thread_config.permissions.permission_policy =
        exo_managed_agents::permissions::PermissionPolicy::AlwaysAsk {};
    let mut agent_config = runtime.get_agent_config(agent.as_ref()).await?;
    agent_config.max_tool_round_trips = Some(1);
    let work = RecoverableTurn {
        streaming: false,
        agent_config,
        thread_config,
        request: request.clone(),
    };
    let turn = thread
        .begin_turn(BeginTurnRequest {
            turn: None,
            session_id: None,
            input: request.input,
            initial_events: vec![work.event()?],
        })
        .await?;
    let turn_id = turn.record().id;
    let session_id = turn.record().session_id;
    let tool_request = ToolRequest {
        namespace: None,
        function_name: "side_effect".into(),
        arguments: Map::new(),
    };
    turn.add_events(vec![
        EventData::Custom {
            event_type: crate::basic::BASIC_TOOL_ROUND.into(),
            payload: serde_json::to_value(crate::basic::BasicToolRound { round: 0 })?,
        },
        EventData::ToolRequested {
            tool_call_id: "call-1".into(),
            response_id: None,
            request: ToolRequest {
                namespace: None,
                function_name: "earlier_tool".into(),
                arguments: Map::new(),
            },
        },
        EventData::Custom {
            event_type: crate::permissions::APPROVAL_REQUESTED.into(),
            payload: serde_json::to_value(crate::permissions::ApprovalRequest {
                approval_id: "approval-0".into(),
                tool_call_id: Some("call-1".into()),
                round: Some(0),
                request: ToolRequest {
                    namespace: None,
                    function_name: "earlier_tool".into(),
                    arguments: Map::new(),
                },
            })?,
        },
    ])
    .await?;
    crate::permissions::respond(
        thread.as_ref(),
        turn.record().clone(),
        &exo_managed_agents::http::protocol::ApprovalResponseBody {
            session_id,
            approval_id: "approval-0".into(),
            approved: true,
            allow_for_tool: false,
        },
    )
    .await?;
    turn.add_events(vec![
        EventData::ToolResult {
            tool_call_id: "call-1".into(),
            result: serde_json::json!({"ok": true}),
        },
        EventData::Custom {
            event_type: crate::basic::BASIC_TOOL_ROUND.into(),
            payload: serde_json::to_value(crate::basic::BasicToolRound { round: 1 })?,
        },
        EventData::ToolRequested {
            tool_call_id: "read-call".into(),
            response_id: None,
            request: ToolRequest {
                namespace: None,
                function_name: "read_file".into(),
                arguments: Map::new(),
            },
        },
        EventData::ToolRequested {
            tool_call_id: "call-1".into(),
            response_id: None,
            request: tool_request.clone(),
        },
        EventData::ToolResult {
            tool_call_id: "read-call".into(),
            result: serde_json::json!({"contents": "saved"}),
        },
        EventData::Custom {
            event_type: crate::permissions::APPROVAL_REQUESTED.into(),
            payload: serde_json::to_value(crate::permissions::ApprovalRequest {
                approval_id: "approval-1".into(),
                tool_call_id: Some("call-1".into()),
                round: Some(1),
                request: tool_request,
            })?,
        },
    ])
    .await?;
    drop(turn);
    drop(thread);
    drop(agent);
    drop(runtime);
    drop(state);

    let state: Arc<dyn ExoHarness> =
        Arc::new(BasicExoHarness::new(local_test_config(&root)).await?);
    let model = Arc::new(FakeModelClient::new(vec![ModelResponse {
        provider_cost_usd: None,
        response_id: None,
        messages: vec![assistant_message("tool done")],
        tool_calls: Vec::new(),
        usage: None,
        model: None,
        ttft: None,
        duration: None,
    }]));
    let tools = Arc::new(CountingToolRuntime::default());
    let runtime = Runtime::new(
        LocalProvider::basic(
            Arc::clone(&state),
            Arc::clone(&model),
            Arc::clone(&tools),
            Arc::new(cost::PricingTable::empty()),
        ),
        None,
    );
    runtime.recover_unfinished_turns().await?;
    let agent = state.get_agent(&agent_id).await?.unwrap();
    let thread = agent.get_thread(&thread_id).await?.unwrap();
    wait_for_active_turn(&runtime, thread.as_ref(), turn_id).await?;
    assert_eq!(tools.0.load(Ordering::SeqCst), 0);
    assert!(model.requests().is_empty());
    runtime
        .approval_response(
            agent_id,
            thread_id,
            turn_id,
            &exo_managed_agents::http::protocol::ApprovalResponseBody {
                session_id,
                approval_id: "approval-1".into(),
                approved: true,
                allow_for_tool: false,
            },
        )
        .await?;
    let events = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let events = thread
                .get_events(Some(EventQuery {
                    turn_id: Some(turn_id),
                    ..Default::default()
                }))
                .await?
                .events;
            if events
                .iter()
                .any(|event| matches!(event.data, EventData::TurnEnded))
            {
                return Ok::<_, anyhow::Error>(events);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await??;
    assert_eq!(tools.0.load(Ordering::SeqCst), 1);
    assert!(model.requests().is_empty());
    assert_eq!(events.iter().filter(|event| matches!(&event.data,
        EventData::Custom { event_type, .. } if event_type == crate::permissions::APPROVAL_REQUESTED
    )).count(), 2);
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event.data, EventData::ToolResult { .. }))
            .count(),
        3
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event.data, EventData::Error { .. }))
    );
    runtime.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn completed_tool_result_survives_restart_before_next_tool_approval() -> Result<()> {
    let tempdir = TempDir::new()?;
    let root = tempdir.path().join("exoharness");
    let state: Arc<dyn ExoHarness> =
        Arc::new(BasicExoHarness::new(local_test_config(&root)).await?);
    create_test_credential(state.as_ref()).await;
    let model = Arc::new(FakeModelClient::new(vec![ModelResponse {
        provider_cost_usd: None,
        response_id: None,
        messages: Vec::new(),
        tool_calls: vec![
            PendingToolCall {
                tool_call_id: "read-call".into(),
                request: ToolRequest {
                    namespace: None,
                    function_name: "read_file".into(),
                    arguments: Map::new(),
                },
            },
            PendingToolCall {
                tool_call_id: "write-call".into(),
                request: ToolRequest {
                    namespace: None,
                    function_name: "write_file".into(),
                    arguments: Map::new(),
                },
            },
        ],
        usage: None,
        model: None,
        ttft: None,
        duration: None,
    }]));
    let first_tools = Arc::new(CountingToolRuntime::default());
    let runtime = Runtime::new(
        LocalProvider::basic(
            Arc::clone(&state),
            model,
            Arc::clone(&first_tools),
            Arc::new(cost::PricingTable::empty()),
        ),
        None,
    );
    let agent = runtime
        .create_agent(CreateAgentRequest {
            ..crate::test_support::agent_request(
                "tool-round-restart",
                crate::AgentHarnessKind::Basic,
            )
        })
        .await?;
    let thread = runtime
        .create_conversation(agent.as_ref(), CreateConversationRequest::default())
        .await?;
    let agent_id = agent.record().id;
    let thread_id = thread.record().id;
    let (turn, mut stream) = runtime
        .start_turn(
            Arc::clone(&agent),
            Arc::clone(&thread),
            SendRequest {
                input: vec![user_message("read then write")],
                session_id: None,
            },
            false,
            None,
        )
        .await?;
    let (approval_id, events) = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let events = thread
                .get_events(Some(EventQuery {
                    turn_id: Some(turn.id),
                    ..Default::default()
                }))
                .await?
                .events;
            if let Some(approval_id) = events.iter().find_map(|event| {
                if let EventData::Custom {
                    event_type,
                    payload,
                } = &event.data
                    && event_type == crate::permissions::APPROVAL_REQUESTED
                {
                    let approval: crate::permissions::ApprovalRequest =
                        serde_json::from_value(payload.clone()).ok()?;
                    return Some(approval.approval_id);
                }
                None
            }) {
                return Ok::<_, anyhow::Error>((approval_id, events));
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await??;
    assert_eq!(first_tools.0.load(Ordering::SeqCst), 1);
    assert!(events.iter().any(|event| matches!(&event.data,
        EventData::ToolResult { tool_call_id, .. } if tool_call_id == "read-call"
    )));
    assert!(!events.iter().any(|event| matches!(&event.data,
        EventData::ToolResult { tool_call_id, .. } if tool_call_id == "write-call"
    )));
    runtime.shutdown().await?;
    assert!(matches!(stream.next().await, Some(Err(_))));
    drop(thread);
    drop(agent);
    drop(runtime);
    drop(state);

    let state: Arc<dyn ExoHarness> =
        Arc::new(BasicExoHarness::new(local_test_config(&root)).await?);
    let model = Arc::new(FakeModelClient::new(vec![ModelResponse {
        provider_cost_usd: None,
        response_id: None,
        messages: vec![assistant_message("write complete")],
        tool_calls: Vec::new(),
        usage: None,
        model: None,
        ttft: None,
        duration: None,
    }]));
    let second_tools = Arc::new(CountingToolRuntime::default());
    let runtime = Runtime::new(
        LocalProvider::basic(
            Arc::clone(&state),
            Arc::clone(&model),
            Arc::clone(&second_tools),
            Arc::new(cost::PricingTable::empty()),
        ),
        None,
    );
    runtime.recover_unfinished_turns().await?;
    let agent = state.get_agent(&agent_id).await?.unwrap();
    let thread = agent.get_thread(&thread_id).await?.unwrap();
    wait_for_active_turn(&runtime, thread.as_ref(), turn.id).await?;
    runtime
        .approval_response(
            agent_id,
            thread_id,
            turn.id,
            &exo_managed_agents::http::protocol::ApprovalResponseBody {
                session_id: turn.session_id,
                approval_id,
                approved: true,
                allow_for_tool: false,
            },
        )
        .await?;
    let events = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let events = thread
                .get_events(Some(EventQuery {
                    turn_id: Some(turn.id),
                    ..Default::default()
                }))
                .await?
                .events;
            if events
                .iter()
                .any(|event| matches!(event.data, EventData::TurnEnded))
            {
                return Ok::<_, anyhow::Error>(events);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await??;
    assert_eq!(second_tools.0.load(Ordering::SeqCst), 1);
    assert_eq!(model.requests().len(), 1);
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event.data, EventData::ToolResult { .. }))
            .count(),
        2
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event.data, EventData::Error { .. }))
    );
    runtime.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn thread_created_during_recovery_can_send_immediately() -> Result<()> {
    let tempdir = TempDir::new()?;
    let state: Arc<dyn ExoHarness> =
        Arc::new(BasicExoHarness::new(local_test_config(tempdir.path().join("exoharness"))).await?);
    create_test_credential(state.as_ref()).await;
    let model = Arc::new(FakeModelClient::new(vec![ModelResponse {
        provider_cost_usd: None,
        response_id: None,
        messages: vec![assistant_message("ready")],
        tool_calls: Vec::new(),
        usage: None,
        model: None,
        ttft: None,
        duration: None,
    }]));
    let runtime = Runtime::new(
        LocalProvider::basic(
            state,
            model,
            Arc::new(BasicToolRuntime),
            Arc::new(cost::PricingTable::empty()),
        ),
        None,
    );
    let agent = runtime
        .create_agent(CreateAgentRequest {
            ..crate::test_support::agent_request(
                "new-during-recovery",
                crate::AgentHarnessKind::Basic,
            )
        })
        .await?;
    runtime.begin_recovery_scan();
    let thread = runtime
        .create_conversation(agent.as_ref(), CreateConversationRequest::default())
        .await?;
    let (_, mut stream) = tokio::time::timeout(
        Duration::from_secs(3),
        runtime.start_turn(
            agent,
            thread,
            SendRequest {
                input: vec![user_message("hello")],
                session_id: None,
            },
            false,
            None,
        ),
    )
    .await??;
    while let Some(event) = stream.next().await {
        event?;
    }
    runtime.recover_unfinished_turns().await?;
    runtime.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn recovery_clears_marker_for_deleted_thread() -> Result<()> {
    let tempdir = TempDir::new()?;
    let root = tempdir.path().join("exoharness");
    let state: Arc<dyn ExoHarness> =
        Arc::new(BasicExoHarness::new(local_test_config(&root)).await?);
    let agent = state
        .new_agent(exoharness::NewAgentRequest {
            slug: "deleted-recovery-thread".to_string(),
            name: "Deleted recovery thread".to_string(),
            vaults: Vec::new(),
        })
        .await?;
    let agent_id = agent.record().id;
    let thread = agent.new_thread(Default::default()).await?;
    let turn = thread.begin_turn(BeginTurnRequest::default()).await?;
    let unfinished = exoharness::ListThreadsRequest {
        unfinished_only: true,
        ..Default::default()
    };
    assert_eq!(
        agent.list_threads(unfinished.clone()).await?.threads.len(),
        1
    );
    agent.delete_thread(&thread.record().id).await?;
    drop(turn);
    drop(thread);
    drop(agent);
    drop(state);

    let state: Arc<dyn ExoHarness> =
        Arc::new(BasicExoHarness::new(local_test_config(&root)).await?);
    let runtime = Runtime::new(
        LocalProvider::basic(
            Arc::clone(&state),
            Arc::new(FakeModelClient::default()),
            Arc::new(BasicToolRuntime),
            Arc::new(cost::PricingTable::empty()),
        ),
        None,
    );
    runtime.recover_unfinished_turns().await?;
    assert!(
        state
            .get_agent(&agent_id)
            .await?
            .expect("agent exists")
            .list_threads(unfinished)
            .await?
            .threads
            .is_empty()
    );
    runtime.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn new_turn_waits_for_older_turn_recovery() -> Result<()> {
    let tempdir = TempDir::new()?;
    let root = tempdir.path().join("exoharness");
    let state: Arc<dyn ExoHarness> =
        Arc::new(BasicExoHarness::new(local_test_config(&root)).await?);
    let runtime = Runtime::new(
        LocalProvider::basic(
            Arc::clone(&state),
            Arc::new(FakeModelClient::default()),
            Arc::new(BasicToolRuntime),
            Arc::new(cost::PricingTable::empty()),
        ),
        None,
    );
    create_test_credential(state.as_ref()).await;
    let agent = runtime
        .create_agent(CreateAgentRequest {
            ..crate::test_support::agent_request("ordered-recovery", crate::AgentHarnessKind::Basic)
        })
        .await?;
    let thread = runtime
        .create_conversation(agent.as_ref(), CreateConversationRequest::default())
        .await?;
    let agent_id = agent.record().id;
    let thread_id = thread.record().id;
    let request = SendRequest {
        input: vec![user_message("older")],
        session_id: None,
    };
    let work = RecoverableTurn {
        streaming: false,
        agent_config: runtime.get_agent_config(agent.as_ref()).await?,
        thread_config: runtime.get_conversation_config(thread.as_ref()).await?,
        request: request.clone(),
    };
    let old_turn = thread
        .begin_turn(BeginTurnRequest {
            turn: None,
            session_id: None,
            input: request.input,
            initial_events: vec![work.event()?],
        })
        .await?;
    let old_turn_id = old_turn.record().id;
    drop(old_turn);
    drop(thread);
    drop(agent);
    drop(runtime);
    drop(state);

    let state: Arc<dyn ExoHarness> =
        Arc::new(BasicExoHarness::new(local_test_config(&root)).await?);
    let model = Arc::new(FakeModelClient::new(
        ["old", "new"]
            .into_iter()
            .map(|text| ModelResponse {
                provider_cost_usd: None,
                response_id: None,
                messages: vec![assistant_message(text)],
                tool_calls: Vec::new(),
                usage: None,
                model: None,
                ttft: None,
                duration: None,
            })
            .collect(),
    ));
    let runtime = Runtime::new(
        LocalProvider::basic(
            Arc::clone(&state),
            Arc::clone(&model),
            Arc::new(BasicToolRuntime),
            Arc::new(cost::PricingTable::empty()),
        ),
        None,
    );
    let agent = state.get_agent(&agent_id).await?.unwrap();
    let thread = agent.get_thread(&thread_id).await?.unwrap();
    runtime.begin_recovery_scan();
    let new_turn = runtime.start_turn(
        Arc::clone(&agent),
        Arc::clone(&thread),
        SendRequest {
            input: vec![user_message("newer")],
            session_id: None,
        },
        false,
        None,
    );
    tokio::pin!(new_turn);
    assert!(
        tokio::time::timeout(Duration::from_millis(20), &mut new_turn)
            .await
            .is_err()
    );
    runtime.recover_unfinished_turns().await?;
    let (new_record, mut stream) = tokio::time::timeout(Duration::from_secs(3), new_turn).await??;
    while let Some(event) = stream.next().await {
        event?;
    }
    let events = thread.get_events(None).await?.events;
    let old_end = events
        .iter()
        .position(|event| {
            event.turn_id == Some(old_turn_id) && matches!(event.data, EventData::TurnEnded)
        })
        .unwrap();
    let new_start = events
        .iter()
        .position(|event| {
            event.turn_id == Some(new_record.id)
                && matches!(event.data, EventData::TurnStarted { .. })
        })
        .unwrap();
    assert!(old_end < new_start);
    assert_eq!(model.requests().len(), 2);
    runtime.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn interrupted_rlm_turn_is_failed_without_replaying_its_input() -> Result<()> {
    let tempdir = TempDir::new()?;
    let root = tempdir.path().join("exoharness");
    let state: Arc<dyn ExoHarness> =
        Arc::new(BasicExoHarness::new(local_test_config(&root)).await?);
    create_test_credential(state.as_ref()).await;
    let blocked_model = Arc::new(BlockingModelClient::default());
    let runtime = Runtime::new(
        LocalProvider::rlm(
            Arc::clone(&state),
            Arc::clone(&blocked_model),
            Arc::new(BasicToolRuntime),
        ),
        None,
    );
    let agent = runtime
        .create_agent(CreateAgentRequest {
            ..crate::test_support::agent_request("interrupted-rlm", crate::AgentHarnessKind::Rlm)
        })
        .await?;
    let thread = runtime
        .create_conversation(agent.as_ref(), CreateConversationRequest::default())
        .await?;
    let agent_id = agent.record().id;
    let thread_id = thread.record().id;
    let (turn, mut stream) = runtime
        .start_turn(
            Arc::clone(&agent),
            Arc::clone(&thread),
            SendRequest {
                input: vec![user_message("do the work")],
                session_id: None,
            },
            false,
            None,
        )
        .await?;
    tokio::time::timeout(Duration::from_secs(3), blocked_model.entered.notified()).await?;
    runtime.shutdown().await?;
    assert!(matches!(stream.next().await, Some(Err(_))));
    drop(thread);
    drop(agent);
    drop(runtime);
    drop(state);

    let state: Arc<dyn ExoHarness> =
        Arc::new(BasicExoHarness::new(local_test_config(&root)).await?);
    let model = Arc::new(FakeModelClient::default());
    let runtime = Runtime::new(
        LocalProvider::rlm(
            Arc::clone(&state),
            Arc::clone(&model),
            Arc::new(BasicToolRuntime),
        ),
        None,
    );
    runtime.recover_unfinished_turns().await?;
    let agent = state.get_agent(&agent_id).await?.unwrap();
    let thread = agent.get_thread(&thread_id).await?.unwrap();
    let events = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let events = thread
                .get_events(Some(EventQuery {
                    turn_id: Some(turn.id),
                    ..Default::default()
                }))
                .await?
                .events;
            if events
                .iter()
                .any(|event| matches!(event.data, EventData::TurnEnded))
            {
                return Ok::<_, anyhow::Error>(events);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await??;
    assert!(model.requests().is_empty());
    assert!(events.iter().any(|event| matches!(&event.data,
        EventData::Error { message, .. } if message.contains("cannot safely resume")
    )));
    runtime.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn graceful_shutdown_leaves_active_turn_for_restart() -> Result<()> {
    let tempdir = TempDir::new()?;
    let root = tempdir.path().join("exoharness");
    let state: Arc<dyn ExoHarness> =
        Arc::new(BasicExoHarness::new(local_test_config(&root)).await?);
    let blocked_model = Arc::new(BlockingModelClient::default());
    let runtime = Arc::new(Runtime::new(
        LocalProvider::basic(
            Arc::clone(&state),
            Arc::clone(&blocked_model),
            Arc::new(BasicToolRuntime),
            Arc::new(cost::PricingTable::empty()),
        ),
        None,
    ));
    create_test_credential(state.as_ref()).await;
    let agent = runtime
        .create_agent(CreateAgentRequest {
            ..crate::test_support::agent_request("shutdown-turn", crate::AgentHarnessKind::Basic)
        })
        .await?;
    let thread = runtime
        .create_conversation(agent.as_ref(), CreateConversationRequest::default())
        .await?;
    let agent_id = agent.record().id;
    let thread_id = thread.record().id;
    let mut config = runtime.get_agent_config(agent.as_ref()).await?;
    config.model = "saved-override".into();
    let (_, mut turn_stream) = runtime
        .start_turn(
            Arc::clone(&agent),
            Arc::clone(&thread),
            SendRequest {
                input: vec![user_message("finish after restart")],
                session_id: None,
            },
            false,
            Some(config),
        )
        .await?;
    tokio::time::timeout(Duration::from_secs(3), blocked_model.entered.notified()).await?;
    runtime.shutdown().await?;
    assert!(matches!(turn_stream.next().await, Some(Err(_))));
    let events = thread.get_events(None).await?.events;
    let turn_id = events
        .iter()
        .find_map(|event| {
            matches!(event.data, EventData::TurnStarted { .. }).then_some(event.turn_id)
        })
        .flatten()
        .expect("turn should have started");
    assert!(
        !events
            .iter()
            .any(|event| matches!(event.data, EventData::TurnEnded))
    );
    drop(thread);
    drop(agent);
    drop(runtime);
    drop(state);

    let state: Arc<dyn ExoHarness> =
        Arc::new(BasicExoHarness::new(local_test_config(&root)).await?);
    let model = Arc::new(FakeModelClient::new(vec![ModelResponse {
        provider_cost_usd: None,
        response_id: None,
        messages: vec![assistant_message("finished")],
        tool_calls: Vec::new(),
        usage: None,
        model: None,
        ttft: None,
        duration: None,
    }]));
    let runtime = Arc::new(Runtime::new(
        LocalProvider::basic(
            state,
            Arc::clone(&model),
            Arc::new(BasicToolRuntime),
            Arc::new(cost::PricingTable::empty()),
        ),
        None,
    ));
    runtime.recover_unfinished_turns().await?;
    let agent = runtime
        .exoharness_handle()
        .get_agent(&agent_id)
        .await?
        .expect("agent should survive restart");
    let thread = agent
        .get_thread(&thread_id)
        .await?
        .expect("thread should survive restart");
    let events = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let events = thread
                .get_events(Some(EventQuery {
                    turn_id: Some(turn_id),
                    ..Default::default()
                }))
                .await?
                .events;
            if events
                .iter()
                .any(|event| matches!(event.data, EventData::TurnEnded))
            {
                return Ok::<_, anyhow::Error>(events);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await??;
    assert_eq!(model.requests().len(), 1);
    assert_eq!(model.requests()[0].model, "saved-override");
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event.data, EventData::TurnStarted { .. }))
            .count(),
        1
    );
    runtime.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn blocked_recovered_turn_does_not_block_http_startup() -> Result<()> {
    let tempdir = TempDir::new()?;
    let root = tempdir.path().join("exoharness");
    let state: Arc<dyn ExoHarness> =
        Arc::new(BasicExoHarness::new(local_test_config(&root)).await?);
    let runtime = Runtime::new(
        LocalProvider::basic(
            Arc::clone(&state),
            Arc::new(FakeModelClient::default()),
            Arc::new(BasicToolRuntime),
            Arc::new(cost::PricingTable::empty()),
        ),
        None,
    );
    create_test_credential(state.as_ref()).await;
    let agent = runtime
        .create_agent(CreateAgentRequest {
            ..crate::test_support::agent_request("blocked-recovery", crate::AgentHarnessKind::Basic)
        })
        .await?;
    let thread = runtime
        .create_conversation(agent.as_ref(), CreateConversationRequest::default())
        .await?;
    let request = SendRequest {
        input: vec![user_message("needs approval")],
        session_id: None,
    };
    let work = RecoverableTurn {
        streaming: false,
        agent_config: runtime.get_agent_config(agent.as_ref()).await?,
        thread_config: runtime.get_conversation_config(thread.as_ref()).await?,
        request: request.clone(),
    };
    thread
        .begin_turn(BeginTurnRequest {
            turn: None,
            session_id: None,
            input: request.input,
            initial_events: vec![work.event()?],
        })
        .await?;
    drop(thread);
    drop(agent);
    drop(runtime);
    drop(state);

    let state: Arc<dyn ExoHarness> =
        Arc::new(BasicExoHarness::new(local_test_config(&root)).await?);
    let model = Arc::new(BlockingModelClient::default());
    let runtime = Arc::new(Runtime::new(
        LocalProvider::basic(
            state,
            Arc::clone(&model),
            Arc::new(BasicToolRuntime),
            Arc::new(cost::PricingTable::empty()),
        ),
        None,
    ));
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let url = format!("http://{}/exo/agent", listener.local_addr()?);
    let service = Arc::new(RuntimeHttpService::new(Arc::clone(&runtime), None)?);
    let server = server(listener, service)?;
    let handle = server.handle();
    tokio::spawn(server);
    tokio::time::timeout(Duration::from_secs(3), model.entered.notified()).await?;
    let response = tokio::time::timeout(Duration::from_secs(3), reqwest::get(url)).await??;
    assert!(response.status().is_success());
    handle.stop(true).await;
    runtime.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn recovered_turn_uses_its_original_caller() -> Result<()> {
    let tempdir = TempDir::new()?;
    let root = tempdir.path().join("exoharness");
    let state: Arc<dyn ExoHarness> =
        Arc::new(BasicExoHarness::new(local_test_config(&root)).await?);
    let runtime = Runtime::new(
        LocalProvider::basic(
            Arc::clone(&state),
            Arc::new(FakeModelClient::default()),
            Arc::new(BasicToolRuntime),
            Arc::new(cost::PricingTable::empty()),
        ),
        None,
    );
    create_test_credential(state.as_ref()).await;
    let agent = runtime
        .create_agent(CreateAgentRequest {
            ..crate::test_support::agent_request("caller-recovery", crate::AgentHarnessKind::Basic)
        })
        .await?;
    let thread = runtime
        .create_conversation(agent.as_ref(), CreateConversationRequest::default())
        .await?;
    let agent_id = agent.record().id;
    let thread_id = thread.record().id;
    let policy: Arc<dyn AccessPolicy> = Arc::new(RecoveryPolicy(
        exoharness::vault::global_vault(state.as_ref())
            .await?
            .record()
            .id,
    ));
    let caller_state = state.with_caller(Caller {
        principal: "alice".into(),
        policy: Arc::clone(&policy),
    })?;
    let caller_agent = caller_state.get_agent(&agent_id).await?.unwrap();
    let caller_thread = caller_agent.get_thread(&thread_id).await?.unwrap();
    let request = SendRequest {
        input: vec![user_message("resume as alice")],
        session_id: None,
    };
    let work = RecoverableTurn {
        streaming: false,
        agent_config: runtime.get_agent_config(agent.as_ref()).await?,
        thread_config: runtime.get_conversation_config(thread.as_ref()).await?,
        request: request.clone(),
    };
    let turn = caller_thread
        .begin_turn(BeginTurnRequest {
            turn: None,
            session_id: None,
            input: request.input,
            initial_events: vec![work.event()?],
        })
        .await?;
    let turn_id = turn.record().id;
    drop(turn);
    drop(caller_thread);
    drop(caller_agent);
    drop(caller_state);
    drop(thread);
    drop(agent);
    drop(runtime);
    drop(state);

    let state: Arc<dyn ExoHarness> =
        Arc::new(BasicExoHarness::new(local_test_config(&root)).await?);
    let recorder = CallerRecordingExecutor::default();
    let runtime = Runtime::new(
        LocalProvider::new(Arc::clone(&state), Arc::new(recorder.clone())),
        None,
    );
    let scoped_runtimes: Arc<Mutex<Vec<Arc<Runtime>>>> = Arc::default();
    let resolver: RecoveryRuntimeResolver = {
        let state = Arc::clone(&state);
        let policy = Arc::clone(&policy);
        let recorder = recorder.clone();
        let scoped_runtimes = Arc::clone(&scoped_runtimes);
        Arc::new(move |principal| {
            let scoped_state = state.with_caller(Caller {
                principal,
                policy: Arc::clone(&policy),
            })?;
            let scoped = Arc::new(Runtime::new(
                LocalProvider::new(scoped_state, Arc::new(recorder.clone())),
                None,
            ));
            scoped_runtimes
                .lock()
                .expect("scoped runtimes poisoned")
                .push(Arc::clone(&scoped));
            Ok(scoped)
        })
    };
    runtime
        .recover_unfinished_turns_with_resolver(Some(resolver))
        .await?;
    let agent = state.get_agent(&agent_id).await?.unwrap();
    let thread = agent.get_thread(&thread_id).await?.unwrap();
    let events = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let events = thread
                .get_events(Some(EventQuery {
                    turn_id: Some(turn_id),
                    ..Default::default()
                }))
                .await?
                .events;
            if events
                .iter()
                .any(|event| matches!(event.data, EventData::TurnEnded))
            {
                return Ok::<_, anyhow::Error>(events);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await??;
    assert!(
        !events
            .iter()
            .any(|event| matches!(event.data, EventData::Error { .. }))
    );
    assert_eq!(
        recorder
            .0
            .lock()
            .expect("caller record poisoned")
            .as_slice(),
        &[Some("alice".into())]
    );
    let scoped = scoped_runtimes
        .lock()
        .expect("scoped runtimes poisoned")
        .drain(..)
        .collect::<Vec<_>>();
    for scoped in scoped {
        scoped.shutdown().await?;
    }
    runtime.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn creates_agents_and_conversations_with_persisted_config() {
    let tempdir = TempDir::new().expect("tempdir should exist");
    let exoharness = Arc::new(
        BasicExoHarness::new(local_test_config(tempdir.path().join("exoharness")))
            .await
            .expect("basic exoharness should initialize"),
    ) as Arc<dyn ExoHarness>;
    let harness = Runtime::new(
        LocalProvider::basic(
            exoharness,
            Arc::new(FakeModelClient::default()),
            Arc::new(BasicToolRuntime),
            Arc::new(cost::PricingTable::empty()),
        ),
        None,
    );
    create_test_credential(harness.exoharness_handle().as_ref()).await;

    let agent = harness
        .create_agent(CreateAgentRequest {
            name: Some("Demo".to_string()),
            sandbox_image: Some("agent-image".to_string()),
            sandbox_provider: SandboxProvider::Docker,
            max_output_tokens: Some(512),
            max_tool_round_trips: Some(3),
            ..crate::test_support::agent_request("demo", crate::AgentHarnessKind::Basic)
        })
        .await
        .expect("agent should be created");
    let conversation = harness
        .create_conversation(
            &*agent,
            CreateConversationRequest {
                slug: Some("session".to_string()),
                name: Some("Session".to_string()),
                ..Default::default()
            },
        )
        .await
        .expect("conversation should be created");

    let stored_agent = harness
        .get_agent("demo")
        .await
        .expect("get agent should succeed")
        .expect("agent should exist");
    let stored_conversation = harness
        .get_conversation(stored_agent.as_ref(), "session")
        .await
        .expect("get conversation should succeed")
        .expect("conversation should exist");

    assert_eq!(stored_agent.record().slug, "demo");
    assert_eq!(
        crate::load_agent_config(&*stored_agent)
            .await
            .expect("agent config")
            .model,
        "gpt-5.4"
    );
    let stored_conversation_config = crate::load_conversation_config(&*stored_conversation)
        .await
        .expect("conversation config");
    assert_eq!(
        stored_conversation_config.shell_program,
        Some("/bin/bash".to_string())
    );
    assert_eq!(
        stored_conversation_config.sandbox_image,
        Some("agent-image".to_string())
    );
    assert_eq!(
        stored_conversation_config.sandbox_provider,
        Some(SandboxProvider::Docker)
    );
    assert_eq!(conversation.record().slug, "session");
}

#[tokio::test(flavor = "current_thread")]
async fn send_persists_messages_through_harness() {
    let tempdir = TempDir::new().expect("tempdir should exist");
    let exoharness = Arc::new(
        BasicExoHarness::new(local_test_config(tempdir.path().join("exoharness")))
            .await
            .expect("basic exoharness should initialize"),
    ) as Arc<dyn ExoHarness>;
    let harness = Runtime::new(
        LocalProvider::basic(
            exoharness,
            Arc::new(FakeModelClient::new(vec![ModelResponse {
                provider_cost_usd: None,
                response_id: Some(Uuid7::now()),
                messages: vec![assistant_message("pong")],
                tool_calls: Vec::new(),
                usage: None,
                model: None,
                ttft: None,
                duration: None,
            }])),
            Arc::new(BasicToolRuntime),
            Arc::new(cost::PricingTable::empty()),
        ),
        None,
    );
    create_test_credential(harness.exoharness_handle().as_ref()).await;

    let agent = harness
        .create_agent(CreateAgentRequest {
            max_tool_round_trips: Some(2),
            ..crate::test_support::agent_request("demo", crate::AgentHarnessKind::Basic)
        })
        .await
        .expect("agent should be created");
    let conversation = harness
        .create_conversation(&*agent, CreateConversationRequest::default())
        .await
        .expect("conversation should be created");

    harness
        .send(
            Arc::clone(&agent),
            Arc::clone(&conversation),
            SendRequest {
                input: vec![user_message("ping")],
                session_id: None,
            },
        )
        .await
        .expect("send should succeed");

    let messages = crate::materialize_conversation_messages(&*conversation)
        .await
        .expect("messages should load");
    assert_eq!(messages.len(), 2);
    assert!(matches!(messages[0], Message::User { .. }));
    assert!(matches!(messages[1], Message::Assistant { .. }));

    let sandbox_events = conversation
        .get_events(Some(EventQuery {
            cursor: None,
            direction: Some(EventQueryDirection::Asc),
            limit: None,
            session_id: None,
            turn_id: None,
            types: Some(vec![EventKind::SANDBOX_CREATED]),
        }))
        .await
        .expect("sandbox events should load")
        .events;
    assert!(
        sandbox_events.is_empty(),
        "plain chat should not provision a sandbox"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn usage_record_is_persisted_with_computed_cost() {
    // Use an inline LiteLLM-schema fixture so the assertion is hermetic
    // and doesn't depend on whatever rates the upstream JSON happens to
    // ship today.
    const PRICING_FIXTURE: &str = r#"{
        "claude-sonnet-4-6": {
            "litellm_provider": "anthropic",
            "mode": "chat",
            "input_cost_per_token": 3e-06,
            "output_cost_per_token": 1.5e-05,
            "cache_read_input_token_cost": 3e-07,
            "cache_creation_input_token_cost": 3.75e-06
        }
    }"#;
    let pricing =
        Arc::new(cost::PricingTable::from_json_str(PRICING_FIXTURE).expect("fixture should parse"));

    let tempdir = TempDir::new().expect("tempdir should exist");
    let exoharness = Arc::new(
        BasicExoHarness::new(local_test_config(tempdir.path().join("exoharness")))
            .await
            .expect("basic exoharness should initialize"),
    ) as Arc<dyn ExoHarness>;
    let harness = Runtime::new(
        LocalProvider::basic(
            Arc::clone(&exoharness),
            Arc::new(FakeModelClient::new(vec![ModelResponse {
                provider_cost_usd: None,
                response_id: Some(Uuid7::now()),
                messages: vec![assistant_message("pong")],
                tool_calls: Vec::new(),
                usage: Some(UniversalUsage {
                    prompt_tokens: Some(1_000),
                    completion_tokens: Some(500),
                    prompt_cached_tokens: None,
                    prompt_cache_creation_tokens: None,
                    completion_reasoning_tokens: None,
                    ..Default::default()
                }),
                model: Some("claude-sonnet-4-6".to_string()),
                ttft: None,
                duration: None,
            }])),
            Arc::new(BasicToolRuntime),
            pricing,
        ),
        None,
    );

    exoharness::vault::global_vault(exoharness.as_ref())
        .await
        .expect("runtime vault")
        .put_secret(PutSecretRequest {
            policy: Some(
                exoharness::CredentialDestination::origin("https://api.anthropic.com")
                    .unwrap()
                    .into(),
            ),
            name: "cost-test-key".to_string(),
            secret: Secret::Key {
                value: "test-key".to_string(),
            },
        })
        .await
        .expect("test secret should register");

    let agent = harness
        .create_agent(CreateAgentRequest {
            credential: Some("cost-test-key".into()),
            base_url: None,
            slug: "cost-demo".to_string(),
            name: None,
            harness: crate::AgentHarnessKind::Basic,
            typescript: None,
            enable_agent_tool_creation: true,
            sandbox_image: None,
            sandbox_provider: SandboxProvider::LocalProcess,
            sandbox_scope: None,
            enable_networking: false,
            model: "claude-sonnet-4-6".to_string(),
            max_output_tokens: None,
            max_tool_round_trips: Some(2),
            braintrust: None,
        })
        .await
        .expect("agent should be created");
    let conversation = harness
        .create_conversation(&*agent, CreateConversationRequest::default())
        .await
        .expect("conversation should be created");

    harness
        .send(
            Arc::clone(&agent),
            Arc::clone(&conversation),
            SendRequest {
                input: vec![user_message("ping")],
                session_id: None,
            },
        )
        .await
        .expect("send should succeed");

    let events = conversation
        .get_events(Some(EventQuery {
            cursor: None,
            direction: Some(EventQueryDirection::Asc),
            limit: None,
            session_id: None,
            turn_id: None,
            types: None,
        }))
        .await
        .expect("get events should succeed")
        .events;

    let assistant_usage = events
        .iter()
        .find_map(|event| match &event.data {
            EventData::Messages {
                messages,
                usage: Some(usage),
                ..
            } if messages
                .iter()
                .any(|m| matches!(m, Message::Assistant { .. })) =>
            {
                Some(usage)
            }
            _ => None,
        })
        .expect("assistant message event should carry a UsageRecord");

    assert_eq!(assistant_usage.model, "claude-sonnet-4-6");
    assert_eq!(assistant_usage.prompt_tokens, Some(1_000));
    assert_eq!(assistant_usage.completion_tokens, Some(500));
    // 1000 prompt @ $3/M + 500 completion @ $15/M = $0.003 + $0.0075 = $0.0105
    let cost = assistant_usage.cost_usd.expect("cost should be computed");
    assert!(
        (cost - 0.0105).abs() < 1e-9,
        "expected cost ~0.0105, got {cost}"
    );
    // Non-streaming path measures total duration.
    assert!(
        assistant_usage.duration_ms.is_some(),
        "duration should be recorded"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn usage_record_with_anthropic_cache_hits() {
    // Anthropic accounting is additive: prompt_tokens is the fresh slice,
    // and cache_read / cache_creation are billed separately on top. The
    // pricing math is unit-tested in exoharness::pricing; this test is the
    // end-to-end proof that cached counts (a) reach the persisted
    // UsageRecord and (b) hit the discounted cache-read rate when
    // compute_cost_usd is invoked through the executor.
    const PRICING_FIXTURE: &str = r#"{
        "claude-sonnet-4-6": {
            "litellm_provider": "anthropic",
            "mode": "chat",
            "input_cost_per_token": 3e-06,
            "output_cost_per_token": 1.5e-05,
            "cache_read_input_token_cost": 3e-07,
            "cache_creation_input_token_cost": 3.75e-06
        }
    }"#;
    let pricing =
        Arc::new(cost::PricingTable::from_json_str(PRICING_FIXTURE).expect("fixture should parse"));

    let tempdir = TempDir::new().expect("tempdir should exist");
    let exoharness = Arc::new(
        BasicExoHarness::new(local_test_config(tempdir.path().join("exoharness")))
            .await
            .expect("basic exoharness should initialize"),
    ) as Arc<dyn ExoHarness>;
    let harness = Runtime::new(
        LocalProvider::basic(
            Arc::clone(&exoharness),
            Arc::new(FakeModelClient::new(vec![ModelResponse {
                provider_cost_usd: None,
                response_id: Some(Uuid7::now()),
                messages: vec![assistant_message("pong")],
                tool_calls: Vec::new(),
                usage: Some(UniversalUsage {
                    prompt_tokens: Some(500),
                    completion_tokens: Some(200),
                    prompt_cached_tokens: Some(10_000),
                    prompt_cache_creation_tokens: Some(2_000),
                    completion_reasoning_tokens: None,
                    ..Default::default()
                }),
                model: Some("claude-sonnet-4-6".to_string()),
                ttft: None,
                duration: None,
            }])),
            Arc::new(BasicToolRuntime),
            pricing,
        ),
        None,
    );

    exoharness::vault::global_vault(exoharness.as_ref())
        .await
        .expect("runtime vault")
        .put_secret(PutSecretRequest {
            policy: Some(
                exoharness::CredentialDestination::origin("https://api.anthropic.com")
                    .unwrap()
                    .into(),
            ),
            name: "anthropic-cache-key".to_string(),
            secret: Secret::Key {
                value: "test-key".to_string(),
            },
        })
        .await
        .expect("test secret should register");

    let agent = harness
        .create_agent(CreateAgentRequest {
            credential: Some("anthropic-cache-key".into()),
            base_url: None,
            slug: "anthropic-cache".to_string(),
            name: None,
            harness: crate::AgentHarnessKind::Basic,
            typescript: None,
            enable_agent_tool_creation: true,
            sandbox_image: None,
            sandbox_provider: SandboxProvider::LocalProcess,
            sandbox_scope: None,
            enable_networking: false,
            model: "claude-sonnet-4-6".to_string(),
            max_output_tokens: None,
            max_tool_round_trips: Some(2),
            braintrust: None,
        })
        .await
        .expect("agent should be created");
    let conversation = harness
        .create_conversation(&*agent, CreateConversationRequest::default())
        .await
        .expect("conversation should be created");

    harness
        .send(
            Arc::clone(&agent),
            Arc::clone(&conversation),
            SendRequest {
                input: vec![user_message("ping")],
                session_id: None,
            },
        )
        .await
        .expect("send should succeed");

    let usage = assistant_usage_record(&conversation).await;

    assert_eq!(usage.model, "claude-sonnet-4-6");
    assert_eq!(usage.prompt_tokens, Some(500));
    assert_eq!(usage.completion_tokens, Some(200));
    assert_eq!(usage.prompt_cached_tokens, Some(10_000));
    assert_eq!(usage.prompt_cache_creation_tokens, Some(2_000));
    // Anthropic-style additive:
    //   500    fresh prompt    @ $3/M     = 0.0015
    //   10000  cache read      @ $0.30/M  = 0.003
    //   2000   cache creation  @ $3.75/M  = 0.0075
    //   200    completion      @ $15/M    = 0.003
    // total = 0.015
    let cost = usage.cost_usd.expect("cost should be computed");
    assert!(
        (cost - 0.015).abs() < 1e-9,
        "expected cost ~0.015, got {cost}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn usage_record_with_openai_inclusive_accounting() {
    // OpenAI accounting is inclusive: prompt_tokens is the *total* input
    // including any cache hits, so the executor must subtract
    // prompt_cached_tokens before billing the fresh-input rate. Getting
    // this wrong silently double-bills cached tokens. This test pins the
    // behavior at the conversation-log level — the same accounting that
    // pricing.rs exercises in isolation now also has to survive the round
    // trip through ModelResponse → UsageRecord → persisted event.
    const PRICING_FIXTURE: &str = r#"{
        "gpt-4o-mini": {
            "litellm_provider": "openai",
            "mode": "chat",
            "input_cost_per_token": 1.5e-07,
            "output_cost_per_token": 6e-07,
            "cache_read_input_token_cost": 7.5e-08
        }
    }"#;
    let pricing =
        Arc::new(cost::PricingTable::from_json_str(PRICING_FIXTURE).expect("fixture should parse"));

    let tempdir = TempDir::new().expect("tempdir should exist");
    let exoharness = Arc::new(
        BasicExoHarness::new(local_test_config(tempdir.path().join("exoharness")))
            .await
            .expect("basic exoharness should initialize"),
    ) as Arc<dyn ExoHarness>;
    let harness = Runtime::new(
        LocalProvider::basic(
            Arc::clone(&exoharness),
            Arc::new(FakeModelClient::new(vec![ModelResponse {
                provider_cost_usd: None,
                response_id: Some(Uuid7::now()),
                messages: vec![assistant_message("pong")],
                tool_calls: Vec::new(),
                usage: Some(UniversalUsage {
                    // prompt_tokens here *includes* the 500 cached — OpenAI
                    // convention.
                    prompt_tokens: Some(2_000),
                    completion_tokens: Some(1_000),
                    prompt_cached_tokens: Some(500),
                    prompt_cache_creation_tokens: None,
                    completion_reasoning_tokens: None,
                    ..Default::default()
                }),
                model: Some("gpt-4o-mini".to_string()),
                ttft: None,
                duration: None,
            }])),
            Arc::new(BasicToolRuntime),
            pricing,
        ),
        None,
    );

    exoharness::vault::global_vault(exoharness.as_ref())
        .await
        .expect("runtime vault")
        .put_secret(PutSecretRequest {
            policy: Some(
                exoharness::CredentialDestination::origin("https://api.openai.com")
                    .unwrap()
                    .into(),
            ),
            name: "openai-cache-key".to_string(),
            secret: Secret::Key {
                value: "test-key".to_string(),
            },
        })
        .await
        .expect("test secret should register");

    let agent = harness
        .create_agent(CreateAgentRequest {
            credential: Some("openai-cache-key".into()),
            base_url: None,
            slug: "openai-cache".to_string(),
            name: None,
            harness: crate::AgentHarnessKind::Basic,
            typescript: None,
            enable_agent_tool_creation: true,
            sandbox_image: None,
            sandbox_provider: SandboxProvider::LocalProcess,
            sandbox_scope: None,
            enable_networking: false,
            model: "gpt-4o-mini".to_string(),
            max_output_tokens: None,
            max_tool_round_trips: Some(2),
            braintrust: None,
        })
        .await
        .expect("agent should be created");
    let conversation = harness
        .create_conversation(&*agent, CreateConversationRequest::default())
        .await
        .expect("conversation should be created");

    harness
        .send(
            Arc::clone(&agent),
            Arc::clone(&conversation),
            SendRequest {
                input: vec![user_message("ping")],
                session_id: None,
            },
        )
        .await
        .expect("send should succeed");

    let usage = assistant_usage_record(&conversation).await;

    assert_eq!(usage.model, "gpt-4o-mini");
    // Raw counts are preserved as the provider reported them — the
    // inclusive convention only matters for the cost computation, not for
    // the stored tokens.
    assert_eq!(usage.prompt_tokens, Some(2_000));
    assert_eq!(usage.completion_tokens, Some(1_000));
    assert_eq!(usage.prompt_cached_tokens, Some(500));
    // OpenAI-style inclusive:
    //   non_cached = 2000 - 500       = 1500
    //   1500   fresh prompt @ $0.15/M = 0.000225
    //   500    cache read   @ $0.075/M = 0.0000375
    //   1000   completion   @ $0.60/M = 0.0006
    // total = 0.0008625
    // If the executor mistakenly used the Anthropic-style additive
    // formula here, it would bill all 2000 prompt tokens at the fresh
    // rate and the total would be 0.0009375 — ~9% high — so this
    // assertion catches the provider-classification bug the PR
    // description calls out.
    let cost = usage.cost_usd.expect("cost should be computed");
    assert!(
        (cost - 0.0008625).abs() < 1e-9,
        "expected cost ~0.0008625, got {cost}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn close_session_appends_session_ended_event() {
    let tempdir = TempDir::new().expect("tempdir should exist");
    let exoharness: Arc<dyn ExoHarness> = Arc::new(
        BasicExoHarness::new(local_test_config(tempdir.path().join("exoharness")))
            .await
            .expect("basic exoharness should initialize"),
    );
    let harness = Runtime::new(
        LocalProvider::basic(
            Arc::clone(&exoharness),
            Arc::new(FakeModelClient::new(vec![ModelResponse {
                provider_cost_usd: None,
                response_id: Some(Uuid7::now()),
                messages: vec![assistant_message("pong")],
                tool_calls: Vec::new(),
                usage: None,
                model: None,
                ttft: None,
                duration: None,
            }])),
            Arc::new(BasicToolRuntime),
            Arc::new(cost::PricingTable::empty()),
        ),
        None,
    );
    create_test_credential(harness.exoharness_handle().as_ref()).await;

    let agent = harness
        .create_agent(CreateAgentRequest {
            max_tool_round_trips: Some(2),
            ..crate::test_support::agent_request("demo", crate::AgentHarnessKind::Basic)
        })
        .await
        .expect("agent should be created");
    let conversation = harness
        .create_conversation(&*agent, CreateConversationRequest::default())
        .await
        .expect("conversation should be created");

    let result = harness
        .send(
            Arc::clone(&agent),
            Arc::clone(&conversation),
            SendRequest {
                input: vec![user_message("ping")],
                session_id: None,
            },
        )
        .await
        .expect("send should succeed");

    conversation
        .end_session(result.session_id)
        .await
        .expect("close session should succeed");

    let events = conversation
        .get_events(Some(EventQuery {
            cursor: None,
            direction: Some(EventQueryDirection::Asc),
            limit: None,
            session_id: Some(result.session_id),
            turn_id: None,
            types: None,
        }))
        .await
        .expect("events should load")
        .events;

    assert!(
        events
            .iter()
            .any(|event| matches!(event.data, EventData::SessionEnded))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn updating_agent_config_refreshes_executor_cache() {
    let tempdir = TempDir::new().expect("tempdir should exist");
    let exoharness = Arc::new(
        BasicExoHarness::new(local_test_config(tempdir.path().join("exoharness")))
            .await
            .expect("basic exoharness should initialize"),
    );
    let model = Arc::new(FakeModelClient::new(vec![
        ModelResponse {
            provider_cost_usd: None,
            response_id: Some(Uuid7::now()),
            messages: vec![assistant_message("pong-1")],
            tool_calls: Vec::new(),
            usage: None,
            model: None,
            ttft: None,
            duration: None,
        },
        ModelResponse {
            provider_cost_usd: None,
            response_id: Some(Uuid7::now()),
            messages: vec![assistant_message("pong-2")],
            tool_calls: Vec::new(),
            usage: None,
            model: None,
            ttft: None,
            duration: None,
        },
    ]));
    let harness = Runtime::new(
        LocalProvider::basic(
            exoharness,
            Arc::clone(&model),
            Arc::new(BasicToolRuntime),
            Arc::new(cost::PricingTable::empty()),
        ),
        None,
    );
    create_test_credential(harness.exoharness_handle().as_ref()).await;

    let agent = harness
        .create_agent(CreateAgentRequest {
            name: Some("Demo".to_string()),
            max_tool_round_trips: Some(2),
            ..crate::test_support::agent_request("demo", crate::AgentHarnessKind::Basic)
        })
        .await
        .expect("agent should be created");
    let conversation = harness
        .create_conversation(&*agent, CreateConversationRequest::default())
        .await
        .expect("conversation should be created");

    harness
        .send(
            Arc::clone(&agent),
            Arc::clone(&conversation),
            SendRequest {
                input: vec![user_message("first")],
                session_id: None,
            },
        )
        .await
        .expect("first send should succeed");

    let mut updated_config = crate::load_agent_config(&*agent)
        .await
        .expect("agent config should load");
    updated_config.model = "gpt-5.4-mini".to_string();
    harness
        .put_agent_config(&*agent, updated_config)
        .await
        .expect("agent config should update");

    harness
        .send(
            Arc::clone(&agent),
            Arc::clone(&conversation),
            SendRequest {
                input: vec![user_message("second")],
                session_id: None,
            },
        )
        .await
        .expect("second send should succeed");

    let requests = model.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].model, "gpt-5.4");
    assert_eq!(requests[1].model, "gpt-5.4-mini");
}

#[tokio::test(flavor = "current_thread")]
async fn send_executes_shell_tool_when_enabled() {
    let tempdir = TempDir::new().expect("tempdir should exist");
    let exoharness = Arc::new(
        BasicExoHarness::new(local_test_config(tempdir.path().join("exoharness")))
            .await
            .expect("basic exoharness should initialize"),
    );
    let model = Arc::new(FakeModelClient::new(vec![
        ModelResponse {
            provider_cost_usd: None,
            response_id: Some(Uuid7::now()),
            messages: Vec::new(),
            tool_calls: vec![PendingToolCall {
                tool_call_id: "call-1".to_string(),
                request: ToolRequest {
                    namespace: None,
                    function_name: "shell".to_string(),
                    arguments: shell_command_arguments("printf hello"),
                },
            }],
            usage: None,
            model: None,
            ttft: None,
            duration: None,
        },
        ModelResponse {
            provider_cost_usd: None,
            response_id: Some(Uuid7::now()),
            messages: vec![assistant_message("done")],
            tool_calls: Vec::new(),
            usage: None,
            model: None,
            ttft: None,
            duration: None,
        },
    ]));
    let harness = Runtime::new(
        LocalProvider::basic(
            exoharness,
            Arc::clone(&model),
            Arc::new(BasicToolRuntime),
            Arc::new(cost::PricingTable::empty()),
        ),
        None,
    );
    create_test_credential(harness.exoharness_handle().as_ref()).await;

    let agent = harness
        .create_agent(CreateAgentRequest {
            name: Some("Demo".to_string()),
            sandbox_image: Some("agent-image".to_string()),
            enable_networking: true,
            max_tool_round_trips: Some(2),
            ..crate::test_support::agent_request("demo", crate::AgentHarnessKind::Basic)
        })
        .await
        .expect("agent should be created");
    let conversation = harness
        .create_conversation(&*agent, CreateConversationRequest::default())
        .await
        .expect("conversation should be created");

    let mut conversation_config = crate::load_conversation_config(&*conversation)
        .await
        .expect("conversation config should load");
    conversation_config.shell_program = Some("/bin/sh".to_string());
    conversation_config.sandbox_image = Some("conversation-image".to_string());
    conversation_config.sandbox_provider = Some(SandboxProvider::LocalProcess);
    harness
        .put_conversation_config(&*conversation, conversation_config)
        .await
        .expect("conversation config should update");

    harness
        .send(
            Arc::clone(&agent),
            Arc::clone(&conversation),
            SendRequest {
                input: vec![user_message("run shell")],
                session_id: None,
            },
        )
        .await
        .expect("send should succeed");

    let messages = crate::materialize_conversation_messages(&*conversation)
        .await
        .expect("messages should load");
    assert!(
        messages
            .iter()
            .any(|message| matches!(message, Message::Tool { .. }))
    );
    assert!(matches!(messages.last(), Some(Message::Assistant { .. })));

    let requests = model.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].tools.len(), 1);
    assert_eq!(requests[0].tools[0].name, "shell");

    let sandbox_events = conversation
        .get_events(Some(EventQuery {
            cursor: None,
            direction: Some(EventQueryDirection::Asc),
            limit: None,
            session_id: None,
            turn_id: None,
            types: Some(vec![EventKind::SANDBOX_CREATED]),
        }))
        .await
        .expect("sandbox events should load")
        .events;
    assert!(matches!(
        &sandbox_events[0].data,
        EventData::SandboxCreated {
            provider,
            image,
            policy: Some(policy),
            enable_networking: true,
            ..
        } if provider == &SandboxProvider::LocalProcess && image == "conversation-image"
            && policy == &exoharness::SandboxNetworkPolicy::Unrestricted.into()
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn harness_exposes_raw_exoharness_handles() {
    let tempdir = TempDir::new().expect("tempdir should exist");
    let exoharness = Arc::new(
        BasicExoHarness::new(local_test_config(tempdir.path().join("exoharness")))
            .await
            .expect("basic exoharness should initialize"),
    );
    let harness = Runtime::new(
        LocalProvider::basic(
            exoharness,
            Arc::new(FakeModelClient::default()),
            Arc::new(BasicToolRuntime),
            Arc::new(cost::PricingTable::empty()),
        ),
        None,
    );
    create_test_credential(harness.exoharness_handle().as_ref()).await;

    let agent = harness
        .create_agent(CreateAgentRequest {
            name: Some("Demo".to_string()),
            ..crate::test_support::agent_request("demo", crate::AgentHarnessKind::Basic)
        })
        .await
        .expect("agent should be created");
    let conversation = harness
        .create_conversation(&*agent, CreateConversationRequest::default())
        .await
        .expect("conversation should be created");

    assert_eq!(
        harness
            .exoharness_handle()
            .list_agents()
            .await
            .expect("list agents through exoharness")
            .len(),
        1
    );
    assert_eq!(
        agent
            .list_conversations(exoharness::ListConversationsRequest::default())
            .await
            .expect("list conversations through agent handle")
            .conversations
            .len(),
        1
    );
    let events = conversation
        .get_events(None)
        .await
        .expect("get events through conversation handle")
        .events;
    assert!(
        events
            .iter()
            .all(|event| event.thread_id == conversation.record().id)
    );
}

#[tokio::test(flavor = "current_thread")]
async fn updating_mounts_recreates_conversation_sandbox() {
    let tempdir = TempDir::new().expect("tempdir should exist");
    let mount_dir = tempdir.path().join("mount");
    std::fs::create_dir_all(&mount_dir).expect("mount dir should exist");

    let exoharness = Arc::new(
        BasicExoHarness::new(local_test_config(tempdir.path().join("exoharness")))
            .await
            .expect("basic exoharness should initialize"),
    );
    let model = Arc::new(FakeModelClient::new(vec![
        ModelResponse {
            provider_cost_usd: None,
            response_id: Some(Uuid7::now()),
            messages: Vec::new(),
            tool_calls: vec![PendingToolCall {
                tool_call_id: "call-1".to_string(),
                request: ToolRequest {
                    namespace: None,
                    function_name: "shell".to_string(),
                    arguments: shell_command_arguments("printf first"),
                },
            }],
            usage: None,
            model: None,
            ttft: None,
            duration: None,
        },
        ModelResponse {
            provider_cost_usd: None,
            response_id: Some(Uuid7::now()),
            messages: vec![assistant_message("done-1")],
            tool_calls: Vec::new(),
            usage: None,
            model: None,
            ttft: None,
            duration: None,
        },
        ModelResponse {
            provider_cost_usd: None,
            response_id: Some(Uuid7::now()),
            messages: Vec::new(),
            tool_calls: vec![PendingToolCall {
                tool_call_id: "call-2".to_string(),
                request: ToolRequest {
                    namespace: None,
                    function_name: "shell".to_string(),
                    arguments: shell_command_arguments("printf second"),
                },
            }],
            usage: None,
            model: None,
            ttft: None,
            duration: None,
        },
        ModelResponse {
            provider_cost_usd: None,
            response_id: Some(Uuid7::now()),
            messages: vec![assistant_message("done-2")],
            tool_calls: Vec::new(),
            usage: None,
            model: None,
            ttft: None,
            duration: None,
        },
    ]));
    let harness = Runtime::new(
        LocalProvider::basic(
            exoharness,
            model,
            Arc::new(BasicToolRuntime),
            Arc::new(cost::PricingTable::empty()),
        ),
        None,
    );
    create_test_credential(harness.exoharness_handle().as_ref()).await;

    let agent = harness
        .create_agent(CreateAgentRequest {
            name: Some("Demo".to_string()),
            enable_networking: true,
            max_tool_round_trips: Some(1),
            ..crate::test_support::agent_request("demo", crate::AgentHarnessKind::Basic)
        })
        .await
        .expect("agent should be created");
    let conversation = harness
        .create_conversation(&*agent, CreateConversationRequest::default())
        .await
        .expect("conversation should be created");

    let mut conversation_config = crate::load_conversation_config(&*conversation)
        .await
        .expect("conversation config should load");
    conversation_config.shell_program = Some("/bin/sh".to_string());
    harness
        .put_conversation_config(&*conversation, conversation_config)
        .await
        .expect("conversation config should update");

    harness
        .send(
            Arc::clone(&agent),
            Arc::clone(&conversation),
            SendRequest {
                input: vec![user_message("first")],
                session_id: None,
            },
        )
        .await
        .expect("first send should succeed");

    let first_sandboxes = conversation
        .get_events(Some(EventQuery {
            cursor: None,
            direction: Some(EventQueryDirection::Asc),
            limit: None,
            session_id: None,
            turn_id: None,
            types: Some(vec![EventKind::SANDBOX_CREATED]),
        }))
        .await
        .expect("get sandbox events")
        .events;
    assert_eq!(first_sandboxes.len(), 1);
    assert!(matches!(
        &first_sandboxes[0].data,
        EventData::SandboxCreated { default_workdir, .. } if default_workdir == "/"
    ));

    let mut updated_config = crate::load_conversation_config(&*conversation)
        .await
        .expect("conversation config should reload");
    updated_config.mounts = vec![FileSystemMount {
        host_path: mount_dir.display().to_string(),
        mount_path: "/mnt/project".to_string(),
        mode: FileSystemMountMode::ReadOnly,
        internal: Some(false),
    }];
    harness
        .put_conversation_config(&*conversation, updated_config)
        .await
        .expect("conversation config should update mounts");

    harness
        .send(
            Arc::clone(&agent),
            Arc::clone(&conversation),
            SendRequest {
                input: vec![user_message("second")],
                session_id: None,
            },
        )
        .await
        .expect("second send should succeed");

    let second_sandboxes = conversation
        .get_events(Some(EventQuery {
            cursor: None,
            direction: Some(EventQueryDirection::Asc),
            limit: None,
            session_id: None,
            turn_id: None,
            types: Some(vec![EventKind::SANDBOX_CREATED]),
        }))
        .await
        .expect("get sandbox events after mount change")
        .events;
    assert_eq!(second_sandboxes.len(), 2);
}

#[tokio::test(flavor = "current_thread")]
async fn updating_sandbox_image_recreates_shell_sandbox_without_shell_program() {
    let tempdir = TempDir::new().expect("tempdir should exist");
    let exoharness = Arc::new(
        BasicExoHarness::new(local_test_config(tempdir.path().join("exoharness")))
            .await
            .expect("basic exoharness should initialize"),
    );
    let harness = Runtime::new(
        LocalProvider::basic(
            exoharness,
            Arc::new(FakeModelClient::default()),
            Arc::new(BasicToolRuntime),
            Arc::new(cost::PricingTable::empty()),
        ),
        None,
    );

    let agent = harness
        .create_agent(CreateAgentRequest {
            name: Some("Demo".to_string()),
            enable_networking: true,
            max_tool_round_trips: Some(1),
            ..crate::test_support::agent_request("demo", crate::AgentHarnessKind::Basic)
        })
        .await
        .expect("agent should be created");
    let conversation = harness
        .create_conversation(&*agent, CreateConversationRequest::default())
        .await
        .expect("conversation should be created");
    let agent_config = crate::load_agent_config(&*agent)
        .await
        .expect("agent config should load");

    let mut conversation_config = crate::load_conversation_config(&*conversation)
        .await
        .expect("conversation config should load");
    conversation_config.shell_program = None;
    conversation_config.sandbox_image = Some("first-image".to_string());
    harness
        .put_conversation_config(&*conversation, conversation_config.clone())
        .await
        .expect("conversation config should update");

    let first_sandbox_id =
        ensure_shell_sandbox(conversation.as_ref(), &agent_config, &conversation_config)
            .await
            .expect("first sandbox should be created");

    conversation_config.shell_program = Some("/missing-shell".into());
    conversation_config.sandbox_scope = Some(crate::SandboxScope::Conversation);
    let native_tools = crate::ExoToolRuntime::with_roots(
        tempdir.path().join("scheduler"),
        tempdir.path().join("adapters"),
        tempdir.path().join("workers"),
    );
    crate::ToolRuntime::prepare_conversation(
        &native_tools,
        agent.as_ref(),
        conversation.as_ref(),
        &agent_config,
        &conversation_config,
    )
    .await
    .expect("native sandbox setup must not depend on a configured shell");
    assert_eq!(conversation.list_sandboxes().await.unwrap().len(), 1);
    conversation_config.shell_program = None;

    conversation_config.sandbox_image = Some("second-image".to_string());
    harness
        .put_conversation_config(&*conversation, conversation_config.clone())
        .await
        .expect("conversation config should update again");

    let second_sandbox_id =
        ensure_shell_sandbox(conversation.as_ref(), &agent_config, &conversation_config)
            .await
            .expect("second sandbox should be created");

    assert_ne!(first_sandbox_id, second_sandbox_id);
    let sandbox_events = conversation
        .get_events(Some(EventQuery {
            cursor: None,
            direction: Some(EventQueryDirection::Asc),
            limit: None,
            session_id: None,
            turn_id: None,
            types: Some(vec![EventKind::SANDBOX_CREATED]),
        }))
        .await
        .expect("get sandbox events")
        .events;
    assert_eq!(sandbox_events.len(), 2);
    assert!(matches!(
        &sandbox_events[0].data,
        EventData::SandboxCreated { image, .. } if image == "first-image"
    ));
    assert!(matches!(
        &sandbox_events[1].data,
        EventData::SandboxCreated { image, .. } if image == "second-image"
    ));

    let attached_sandbox_id = "borrowed-docker-sandbox".to_string();
    conversation
        .add_events(AddEventsRequest {
            session_id: None,
            turn_id: None,
            data: vec![EventData::SandboxAttached {
                sandbox_id: attached_sandbox_id.clone(),
                attachment: SandboxAttachment::DockerContainer {
                    container_id: "docker-container".to_string(),
                },
                default_workdir: "/workspace".to_string(),
            }],
        })
        .await
        .expect("sandbox attachment event should be recorded");
    assert_eq!(
        ensure_shell_sandbox(conversation.as_ref(), &agent_config, &conversation_config,)
            .await
            .expect("an attachment event without a current sandbox record must be ignored"),
        second_sandbox_id
    );

    conversation
        .add_events(AddEventsRequest {
            session_id: None,
            turn_id: None,
            data: vec![EventData::SandboxDetached {
                sandbox_id: attached_sandbox_id,
                attachment: SandboxAttachment::DockerContainer {
                    container_id: "docker-container".to_string(),
                },
            }],
        })
        .await
        .expect("sandbox detachment event should be recorded");
    assert_eq!(
        ensure_shell_sandbox(conversation.as_ref(), &agent_config, &conversation_config,)
            .await
            .expect("previous sandbox should be selected after detachment"),
        second_sandbox_id
    );
}

#[tokio::test(flavor = "current_thread")]
async fn conversation_model_override_changes_effective_model() {
    let tempdir = TempDir::new().expect("tempdir should exist");
    let exoharness: Arc<dyn ExoHarness> = Arc::new(
        BasicExoHarness::new(local_test_config(tempdir.path().join("exoharness")))
            .await
            .expect("basic exoharness should initialize"),
    );
    let model = Arc::new(FakeModelClient::new(vec![
        ModelResponse {
            provider_cost_usd: None,
            response_id: Some(Uuid7::now()),
            messages: vec![assistant_message("first")],
            tool_calls: Vec::new(),
            usage: None,
            model: None,
            ttft: None,
            duration: None,
        },
        ModelResponse {
            provider_cost_usd: None,
            response_id: Some(Uuid7::now()),
            messages: vec![assistant_message("second")],
            tool_calls: Vec::new(),
            usage: None,
            model: None,
            ttft: None,
            duration: None,
        },
        ModelResponse {
            provider_cost_usd: None,
            response_id: Some(Uuid7::now()),
            messages: vec![assistant_message("third")],
            tool_calls: Vec::new(),
            usage: None,
            model: None,
            ttft: None,
            duration: None,
        },
    ]));
    let harness = Runtime::new(
        LocalProvider::basic(
            exoharness,
            Arc::clone(&model),
            Arc::new(BasicToolRuntime),
            Arc::new(cost::PricingTable::empty()),
        ),
        None,
    );
    create_test_credential(harness.exoharness_handle().as_ref()).await;

    let agent = harness
        .create_agent(CreateAgentRequest {
            max_output_tokens: Some(512),
            max_tool_round_trips: Some(2),
            ..crate::test_support::agent_request("demo", crate::AgentHarnessKind::Basic)
        })
        .await
        .expect("agent should be created");
    let conversation = harness
        .create_conversation(&*agent, CreateConversationRequest::default())
        .await
        .expect("conversation should be created");

    harness
        .send(
            Arc::clone(&agent),
            Arc::clone(&conversation),
            SendRequest {
                input: vec![user_message("first")],
                session_id: None,
            },
        )
        .await
        .expect("first send should succeed");

    crate::put_conversation_model_override(
        &*conversation,
        Some(ConversationModelConfig {
            model: "claude-sonnet-4".to_string(),
            max_output_tokens: Some(2048),
        }),
    )
    .await
    .expect("model override should persist");

    assert_eq!(
        crate::get_conversation_model_override(&*conversation)
            .await
            .expect("model override should load"),
        Some(ConversationModelConfig {
            model: "claude-sonnet-4".to_string(),
            max_output_tokens: Some(2048),
        })
    );

    harness
        .send(
            Arc::clone(&agent),
            Arc::clone(&conversation),
            SendRequest {
                input: vec![user_message("second")],
                session_id: None,
            },
        )
        .await
        .expect("second send should succeed");

    crate::put_conversation_model_override(&*conversation, None)
        .await
        .expect("model override should clear");

    harness
        .send(
            Arc::clone(&agent),
            Arc::clone(&conversation),
            SendRequest {
                input: vec![user_message("third")],
                session_id: None,
            },
        )
        .await
        .expect("third send should succeed");

    let requests = model.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[0].model, "gpt-5.4");
    assert_eq!(requests[0].max_output_tokens, Some(512));
    assert_eq!(requests[1].model, "claude-sonnet-4");
    assert_eq!(requests[1].max_output_tokens, Some(2048));
    assert_eq!(requests[2].model, "gpt-5.4");
    assert_eq!(requests[2].max_output_tokens, Some(512));
}

#[derive(Default)]
struct FakeModelClient {
    responses: Mutex<VecDeque<ModelResponse>>,
    requests: Mutex<Vec<ModelRequest>>,
}

#[derive(Default)]
struct BlockingModelClient {
    entered: tokio::sync::Notify,
}

#[async_trait]
impl ModelClient for BlockingModelClient {
    async fn complete(&self, _request: ModelRequest) -> Result<ModelResponse> {
        self.entered.notify_one();
        std::future::pending().await
    }

    async fn complete_stream(
        &self,
        _request: ModelRequest,
    ) -> Result<Box<dyn ModelResponseStream>> {
        Err(anyhow!("streaming is not expected"))
    }
}

impl FakeModelClient {
    fn new(responses: Vec<ModelResponse>) -> Self {
        Self {
            responses: Mutex::new(VecDeque::from(responses)),
            requests: Mutex::new(Vec::new()),
        }
    }

    fn requests(&self) -> Vec<ModelRequest> {
        self.requests.lock().expect("model client poisoned").clone()
    }
}

#[async_trait]
impl ModelClient for FakeModelClient {
    async fn complete(&self, request: ModelRequest) -> Result<ModelResponse> {
        self.requests
            .lock()
            .expect("model client poisoned")
            .push(request);
        let mut responses = self.responses.lock().expect("model client poisoned");
        responses
            .pop_front()
            .ok_or_else(|| anyhow!("no more model responses configured"))
    }

    async fn complete_stream(
        &self,
        _request: ModelRequest,
    ) -> Result<Box<dyn ModelResponseStream>> {
        Ok(Box::new(FakeModelResponseStream))
    }
}

struct FakeModelResponseStream;

#[async_trait]
impl ModelResponseStream for FakeModelResponseStream {
    async fn next_chunk(&mut self) -> Result<Option<UniversalStreamChunk>> {
        Ok(None)
    }

    async fn finish(self: Box<Self>) -> Result<ModelResponse> {
        Err(anyhow!("streaming not configured"))
    }
}

fn user_message(text: &str) -> Message {
    Message::User {
        content: UserContent::String(text.to_string()),
    }
}

fn assistant_message(text: &str) -> Message {
    Message::Assistant {
        content: AssistantContent::String(text.to_string()),
        id: None,
    }
}

/// Fetch the UsageRecord attached to the first Messages event that
/// contains an assistant message. Mirrors the pattern in
/// `usage_record_is_persisted_with_computed_cost` so the new tests stay
/// readable.
async fn assistant_usage_record(
    conversation: &Arc<dyn crate::ConversationHandle>,
) -> exoharness::UsageRecord {
    let events = conversation
        .get_events(Some(EventQuery {
            cursor: None,
            direction: Some(EventQueryDirection::Asc),
            limit: None,
            session_id: None,
            turn_id: None,
            types: None,
        }))
        .await
        .expect("get events should succeed")
        .events;

    events
        .into_iter()
        .find_map(|event| match event.data {
            EventData::Messages {
                messages,
                usage: Some(usage),
                ..
            } if messages
                .iter()
                .any(|m| matches!(m, Message::Assistant { .. })) =>
            {
                Some(*usage)
            }
            _ => None,
        })
        .expect("assistant message event should carry a UsageRecord")
}

fn shell_command_arguments(command: &str) -> Map<String, Value> {
    Map::from_iter([(String::from("command"), Value::String(command.to_string()))])
}

#[tokio::test]
async fn remote_threads_paginate_and_failed_creation_only_deletes_the_new_thread() -> Result<()> {
    use exoharness::protocol::{
        ClientMessage, ConversationHandleInfo, Request, Response, ServerMessage,
    };
    use exoharness::{HttpExoHarness, ListConversationsResult, ReadArtifactRequest};
    use wiremock::{Mock, MockServer, ResponseTemplate, matchers::path};

    let temp = TempDir::new()?;
    let local = Runtime::new(
        LocalProvider::basic(
            Arc::new(BasicExoHarness::new(local_test_config(temp.path())).await?),
            Arc::new(FakeModelClient::default()),
            Arc::new(BasicToolRuntime),
            Arc::new(cost::PricingTable::empty()),
        ),
        None,
    );
    let agent = local
        .create_agent(CreateAgentRequest {
            enable_agent_tool_creation: false,
            model: "gpt-test".to_string(),
            ..crate::test_support::agent_request("support", crate::AgentHarnessKind::Basic)
        })
        .await?;
    let artifact = agent.list_artifacts().await?.remove(0);
    let config_artifact = agent
        .read_artifact(ReadArtifactRequest {
            artifact_id: artifact.artifact_id,
            version: Some(artifact.version),
        })
        .await?
        .ok_or_else(|| anyhow!("missing test agent config"))?;
    let agent_record = agent.record().clone();
    let agent_id = agent_record.id;
    let mut threads = Vec::new();
    for slug in ["failed", "older", "middle", "recent"] {
        let thread = local
            .create_conversation(
                agent.as_ref(),
                CreateConversationRequest {
                    slug: Some(slug.to_string()),
                    ..Default::default()
                },
            )
            .await?;
        threads.push(ConversationHandleInfo {
            agent_id,
            record: thread.record().clone(),
        });
    }
    threads.reverse();
    let recent_id = threads[0].record.id;
    let failed_id = threads[3].record.id;
    let invalid_cursor = Arc::new(Mutex::new(None::<Uuid7>));
    let cursor_override = invalid_cursor.clone();
    let deleted = Arc::new(Mutex::new(Vec::new()));
    let deletions = deleted.clone();
    let server = MockServer::start().await;
    Mock::given(path("/request"))
        .respond_with(move |request: &wiremock::Request| {
            let ClientMessage::Request { id, request } =
                serde_json::from_slice(&request.body).unwrap();
            let response = match request {
                Request::GetAgent { .. } => Some(Response::Agent {
                    agent: Some(agent_record.clone()),
                }),
                Request::AgentWriteArtifact { .. } => Some(Response::ArtifactVersion {
                    artifact: artifact.clone(),
                }),
                Request::AgentListArtifacts { .. } => Some(Response::ArtifactVersions {
                    artifacts: vec![artifact.clone()],
                }),
                Request::AgentReadArtifact { .. } => Some(Response::Artifact {
                    artifact: Some(config_artifact.clone()),
                }),
                Request::ListConversations { request, .. } => {
                    let index = request.cursor.map_or(0, |cursor| {
                        threads
                            .iter()
                            .position(|thread| thread.record.id == cursor)
                            .unwrap()
                            + 1
                    });
                    let mut next_cursor = (index < 2).then_some(threads[index].record.id);
                    if request.cursor.is_some() {
                        next_cursor = cursor_override.lock().unwrap().or(next_cursor);
                    }
                    Some(Response::Conversations {
                        result: ListConversationsResult {
                            conversations: vec![threads[index].clone()],
                            next_cursor,
                        },
                    })
                }
                Request::NewConversation { .. } => Some(Response::Conversation {
                    conversation: Some(threads[3].clone()),
                }),
                Request::ConversationWriteArtifact { .. } => None,
                Request::DeleteConversation {
                    conversation_id, ..
                } => {
                    deletions.lock().unwrap().push(conversation_id);
                    Some(Response::Bool { value: true })
                }
                other => panic!("unexpected request: {other:?}"),
            };
            ResponseTemplate::new(200).set_body_json(ServerMessage::Response {
                id,
                ok: response.is_some(),
                error: response
                    .is_none()
                    .then(|| "configuration write failed".to_string()),
                response,
            })
        })
        .mount(&server)
        .await;
    let remote = Runtime::new(
        LocalProvider::basic(
            Arc::new(HttpExoHarness::new(server.uri(), None)?),
            Arc::new(FakeModelClient::default()),
            Arc::new(BasicToolRuntime),
            Arc::new(cost::PricingTable::empty()),
        ),
        None,
    );
    let agent = remote.get_agent(&agent_id.to_string()).await?.unwrap();
    assert_eq!(
        crate::harness_helpers::list_conversation_handles(agent.as_ref())
            .await?
            .len(),
        3
    );
    assert_eq!(
        remote
            .get_conversation(agent.as_ref(), "older")
            .await?
            .unwrap()
            .record()
            .slug,
        "older"
    );
    let error = remote
        .create_conversation(agent.as_ref(), CreateConversationRequest::default())
        .await
        .err()
        .unwrap();
    assert!(error.to_string().contains("configuration write failed"));
    assert_eq!(*deleted.lock().unwrap(), vec![failed_id]);
    for cursor in [recent_id, Uuid7::now()] {
        *invalid_cursor.lock().unwrap() = Some(cursor);
        let error = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            crate::harness_helpers::list_conversation_handles(agent.as_ref()),
        )
        .await?
        .err()
        .unwrap();
        assert!(
            error
                .to_string()
                .contains("thread listing cursor did not advance")
        );
    }
    Ok(())
}

#[tokio::test]
async fn basic_and_rlm_models_receive_mcp_errors_and_can_continue() -> Result<()> {
    use exo_mcp::{McpCredentials, McpServerConfig, McpToolSet};
    use serde_json::json;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{body_partial_json, method},
    };

    #[derive(serde::Deserialize)]
    struct Rpc {
        id: u64,
    }
    let server = MockServer::start().await;
    for (method_name, result) in [
        (
            "initialize",
            json!({"protocolVersion":"2025-11-25","capabilities":{"tools":{}},"serverInfo":{"name":"fixture","version":"1"}}),
        ),
        (
            "tools/list",
            json!({"tools":[{"name":"search","inputSchema":{"type":"object"}}]}),
        ),
        (
            "tools/call",
            json!({"isError":true,"content":[{"type":"text","text":"Please supply a query"}],"structuredContent":{"reason":"missing_query"}}),
        ),
    ] {
        Mock::given(method("POST"))
            .and(body_partial_json(json!({"method":method_name})))
            .respond_with(move |request: &wiremock::Request| {
                let rpc: Rpc = request.body_json().unwrap();
                ResponseTemplate::new(200)
                    .set_body_json(json!({"jsonrpc":"2.0","id":rpc.id,"result":result}))
            })
            .mount(&server)
            .await;
    }
    Mock::given(method("POST"))
        .and(body_partial_json(
            json!({"method":"notifications/initialized"}),
        ))
        .respond_with(ResponseTemplate::new(202))
        .mount(&server)
        .await;
    let mcp = Arc::new(
        McpToolSet::connect(
            &[McpServerConfig {
                name: "fixture".into(),
                url: server.uri(),
                allowed_tools: None,
                blocked_tools: vec![],
            }],
            McpCredentials::default(),
        )
        .await?,
    );
    for kind in [crate::AgentHarnessKind::Basic, crate::AgentHarnessKind::Rlm] {
        let temp = TempDir::new()?;
        let root =
            Arc::new(BasicExoHarness::new(local_test_config(temp.path().join("state"))).await?);
        let model = Arc::new(FakeModelClient::new(vec![
            serde_json::from_value(
                json!({"messages":[],"tool_calls":[{"tool_call_id":"search-1","request":{"function_name":"exo_mcp__fixture__search","arguments":{}}}]}),
            )?,
            serde_json::from_value(
                json!({"messages":[{"role":"assistant","content":"Please supply a query."}],"tool_calls":[]}),
            )?,
        ]));
        let tools = Arc::new(crate::McpToolRuntime::new(
            BasicToolRuntime,
            Arc::clone(&mcp),
        ));
        let provider = match kind {
            crate::AgentHarnessKind::Basic => LocalProvider::basic(
                root,
                Arc::clone(&model),
                tools,
                Arc::new(cost::PricingTable::empty()),
            ),
            _ => LocalProvider::rlm(root, Arc::clone(&model), tools),
        };
        let harness = Runtime::new(provider, None);
        create_test_credential(harness.exoharness_handle().as_ref()).await;
        let agent = harness
            .create_agent(CreateAgentRequest {
                enable_agent_tool_creation: false,
                enable_networking: true,
                max_tool_round_trips: Some(1),
                ..crate::test_support::agent_request("mcp-errors", kind)
            })
            .await?;
        let thread = harness
            .create_conversation(agent.as_ref(), CreateConversationRequest::default())
            .await?;
        let mut config = harness.get_conversation_config(thread.as_ref()).await?;
        config.permissions.tool_policies.insert(
            "exo_mcp__fixture__search".into(),
            exo_managed_agents::permissions::PermissionPolicy::AlwaysAllow {},
        );
        harness
            .put_conversation_config(thread.as_ref(), config)
            .await?;
        harness
            .send(
                agent,
                thread,
                SendRequest {
                    input: vec![user_message("Search")],
                    session_id: None,
                },
            )
            .await?;
        let requests = model.requests();
        assert_eq!(requests.len(), 2);
        let output = requests[1]
            .messages
            .iter()
            .find_map(|message| {
                if let Message::Tool { content } = message {
                    let lingua::universal::ToolContentPart::ToolResult(result) = &content[0];
                    Some(&result.output)
                } else {
                    None
                }
            })
            .expect("model should receive the MCP error result");
        assert_eq!(
            lingua::serde_json::to_string(output)?,
            json!({
                "isError":true,"content":[{"type":"text","text":"Please supply a query"}],
                "structuredContent":{"reason":"missing_query"}
            })
            .to_string()
        );
        harness.shutdown().await?;
    }
    mcp.close().await?;
    Ok(())
}
