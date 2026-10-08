use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use exoharness::{
    AgentHandle, BasicExoHarness, ConversationHandle, EventData, ExoHarness, NewAgentRequest,
    NewThreadRequest, TurnHandle,
};
use tempfile::TempDir;
use tokio::sync::{Notify, Semaphore, mpsc};
use tokio_stream::StreamExt;

use crate::execution_tracing::TurnExecutionTrace;
use crate::harness::{
    Harness, HarnessCommand, HarnessEvent, HarnessEventAck, HarnessEventHandler, HarnessEventSink,
    HarnessTurnKey, HarnessTurnOutcome,
};
use crate::harness_adapter::{ExecutorHarness, ExecutorTurn};
use crate::harness_events::{HarnessEvents, HarnessTurn};
use crate::harness_executor::{ExecutorStreamMode, HarnessExecutor, Runtime};
use crate::{
    AgentConfig, AgentHarnessKind, AgentSandboxConfig, ConversationConfig, SandboxProvider,
    SendRequest,
};

#[derive(Clone)]
struct ControlledExecutor {
    started: Arc<Notify>,
    release: Arc<Semaphore>,
    cancelling: Arc<Notify>,
    cleanup: Arc<Semaphore>,
    running: Arc<AtomicUsize>,
    fail_cleanup: bool,
}

impl Default for ControlledExecutor {
    fn default() -> Self {
        Self {
            started: Arc::default(),
            release: Arc::new(Semaphore::new(0)),
            cancelling: Arc::default(),
            cleanup: Arc::new(Semaphore::new(0)),
            running: Arc::default(),
            fail_cleanup: false,
        }
    }
}

struct Running(Arc<AtomicUsize>);
impl Drop for Running {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

#[async_trait]
impl HarnessExecutor for ControlledExecutor {
    fn name(&self) -> &'static str {
        "controlled"
    }
    async fn execute_turn(
        &self,
        _: &dyn AgentHandle,
        _: Arc<dyn ConversationHandle>,
        _: Arc<dyn TurnHandle>,
        _: &AgentConfig,
        _: &ConversationConfig,
        request: &SendRequest,
        _: ExecutorStreamMode<'_>,
        _: Option<&dyn TurnExecutionTrace>,
    ) -> Result<()> {
        assert_eq!(self.running.fetch_add(1, Ordering::SeqCst), 0);
        let _running = Running(Arc::clone(&self.running));
        self.started.notify_one();
        assert!(request.input.is_empty(), "test harness panic");
        self.release.acquire().await?.forget();
        Ok(())
    }

    async fn cancel_turn(&self, _: &dyn ConversationHandle, _: &AgentConfig) -> Result<()> {
        self.cancelling.notify_one();
        if self.fail_cleanup {
            anyhow::bail!("cleanup failed");
        }
        self.cleanup.acquire().await?.forget();
        Ok(())
    }
}

struct Fixture {
    _temp: TempDir,
    storage: Arc<dyn ExoHarness>,
    agent: Arc<dyn AgentHandle>,
    thread: Arc<dyn ConversationHandle>,
    config: AgentConfig,
}

impl Fixture {
    async fn new() -> Result<Self> {
        let temp = TempDir::new()?;
        let storage: Arc<dyn ExoHarness> = Arc::new(
            BasicExoHarness::new(crate::test_support::local_test_config(temp.path())).await?,
        );
        let agent = storage
            .new_agent(NewAgentRequest {
                vaults: vec![],
                slug: "test".to_string(),
                name: "Test".to_string(),
            })
            .await?;
        let thread = agent
            .new_thread(NewThreadRequest {
                environment: None,
                vaults: vec![],
                slug: Some("test".to_string()),
                name: Some("Test".to_string()),
            })
            .await?;
        let config = AgentConfig {
            frontend_tools: Vec::new(),
            credential: Some("test-openai".into()),
            base_url: None,
            reasoning_effort: None,
            resources: Vec::new(),
            instructions: Vec::new(),
            harness: AgentHarnessKind::Basic,
            typescript: None,
            enable_agent_tool_creation: false,
            sandbox: AgentSandboxConfig {
                scope: Default::default(),
                image: None,
                provider: SandboxProvider::LocalProcess,
                mounts: Vec::new(),
                enable_networking: false,
            },
            model: "test-model".to_string(),
            max_output_tokens: None,
            max_tool_round_trips: None,
            braintrust: None,
        };
        crate::harness_config::store_agent_config(agent.as_ref(), &config).await?;
        Ok(Self {
            _temp: temp,
            storage,
            agent,
            thread,
            config,
        })
    }

    fn work(&self, turn: Arc<dyn TurnHandle>, panic: bool) -> ExecutorTurn {
        ExecutorTurn {
            agent: Arc::clone(&self.agent),
            thread: Arc::clone(&self.thread),
            turn,
            agent_config: self.config.clone(),
            thread_config: ConversationConfig::default(),
            request: SendRequest {
                input: if panic {
                    vec![crate::harness_helpers::user_message("panic")]
                } else {
                    Vec::new()
                },
                session_id: None,
            },
            recovering: false,
            stream: None,
            trace: None,
        }
    }

    fn request(&self) -> SendRequest {
        SendRequest {
            input: Vec::new(),
            session_id: None,
        }
    }
}

struct Recorder(mpsc::UnboundedSender<HarnessEvent>);
#[async_trait]
impl HarnessEventHandler for Recorder {
    async fn emit(&self, event: HarnessEvent) -> Result<HarnessEventAck> {
        self.0
            .send(event)
            .map_err(|_| anyhow::anyhow!("recorder closed"))?;
        Ok(HarnessEventAck::default())
    }
}

#[tokio::test]
async fn submission_is_nonblocking_and_cancellation_waits_for_cleanup() -> Result<()> {
    let fixture = Fixture::new().await?;
    let executor = ControlledExecutor::default();
    let harness = ExecutorHarness::new(
        Arc::new(executor.clone()),
        Arc::new(crate::TokioRuntimeHost),
    );
    let (tx, mut events) = mpsc::unbounded_channel();
    harness
        .init(HarnessEventSink::new(Arc::new(Recorder(tx))))
        .await?;
    let turn = exoharness::test_support::begin_test_turn(fixture.thread.as_ref()).await?;
    let key = HarnessTurnKey {
        agent_id: fixture.agent.record().id,
        thread_id: fixture.thread.record().id,
        turn_id: turn.record().id,
    };
    tokio::time::timeout(
        Duration::from_secs(1),
        harness.submit(HarnessCommand::StartTurn(
            fixture.work(Arc::clone(&turn), false),
        )),
    )
    .await??;
    executor.started.notified().await;
    assert!(
        harness
            .submit(HarnessCommand::StartTurn(fixture.work(turn, false)))
            .await
            .is_err()
    );
    harness.submit(HarnessCommand::CancelTurn { key }).await?;
    executor.cancelling.notified().await;
    assert!(events.try_recv().is_err());
    executor.cleanup.add_permits(1);
    assert!(matches!(
        events.recv().await,
        Some(HarnessEvent::TurnFinished {
            outcome: HarnessTurnOutcome::Cancelled,
            ..
        })
    ));
    assert!(matches!(
        events.recv().await,
        Some(HarnessEvent::ExecutionStopped { .. })
    ));
    harness.shutdown().await?;
    let turn = exoharness::test_support::begin_test_turn(fixture.thread.as_ref()).await?;
    assert!(
        harness
            .submit(HarnessCommand::StartTurn(fixture.work(turn, false)))
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn failed_shutdown_cleanup_keeps_turn_interrupted() -> Result<()> {
    let fixture = Fixture::new().await?;
    let executor = ControlledExecutor {
        fail_cleanup: true,
        ..Default::default()
    };
    let harness = Arc::new(ExecutorHarness::new(
        Arc::new(executor.clone()),
        Arc::new(crate::TokioRuntimeHost),
    ));
    let (tx, mut events) = mpsc::unbounded_channel();
    harness
        .init(HarnessEventSink::new(Arc::new(Recorder(tx))))
        .await?;
    let turn = exoharness::test_support::begin_test_turn(fixture.thread.as_ref()).await?;
    harness
        .submit(HarnessCommand::StartTurn(fixture.work(turn, false)))
        .await?;
    executor.started.notified().await;
    let shutdown = {
        let harness = Arc::clone(&harness);
        tokio::spawn(async move { harness.shutdown().await })
    };
    assert!(matches!(
        events.recv().await,
        Some(HarnessEvent::TurnFinished {
            outcome: HarnessTurnOutcome::Interrupted,
            ..
        })
    ));
    assert!(matches!(
        events.recv().await,
        Some(HarnessEvent::ExecutionStopped { .. })
    ));
    shutdown.await??;
    Ok(())
}

#[tokio::test]
async fn panic_reports_failure_and_releases_execution() -> Result<()> {
    let fixture = Fixture::new().await?;
    let harness = ExecutorHarness::new(
        Arc::new(ControlledExecutor::default()),
        Arc::new(crate::TokioRuntimeHost),
    );
    let (tx, mut events) = mpsc::unbounded_channel();
    harness
        .init(HarnessEventSink::new(Arc::new(Recorder(tx))))
        .await?;
    let turn = exoharness::test_support::begin_test_turn(fixture.thread.as_ref()).await?;
    harness
        .submit(HarnessCommand::StartTurn(fixture.work(turn, true)))
        .await?;
    assert!(matches!(
        events.recv().await,
        Some(HarnessEvent::TurnFinished {
            outcome: HarnessTurnOutcome::Failed(_),
            ..
        })
    ));
    assert!(matches!(
        events.recv().await,
        Some(HarnessEvent::ExecutionStopped { .. })
    ));
    harness.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn queued_turn_waits_for_cancelled_execution_cleanup() -> Result<()> {
    let fixture = Fixture::new().await?;
    let executor = ControlledExecutor::default();
    let runtime = Runtime::new(
        crate::LocalProvider::new(Arc::clone(&fixture.storage), Arc::new(executor.clone())),
        None,
    );
    let (first_turn, first) = runtime
        .start_turn(
            Arc::clone(&fixture.agent),
            Arc::clone(&fixture.thread),
            fixture.request(),
            true,
            None,
            Default::default(),
        )
        .await?;
    executor.started.notified().await;
    drop(first);
    runtime
        .cancel_turn(HarnessTurnKey::new(
            fixture.agent.record().id,
            fixture.thread.record().id,
            first_turn.id,
        ))
        .await?;
    executor.cancelling.notified().await;
    let mut second = runtime
        .send_stream(
            Arc::clone(&fixture.agent),
            Arc::clone(&fixture.thread),
            fixture.request(),
        )
        .await?;
    assert!(
        tokio::time::timeout(Duration::from_millis(20), executor.started.notified())
            .await
            .is_err()
    );
    executor.cleanup.add_permits(1);
    tokio::time::timeout(Duration::from_secs(1), executor.started.notified()).await?;
    executor.release.add_permits(1);
    assert!(matches!(
        second.next().await.transpose()?,
        Some(crate::ExecutionStreamEvent::Completed(_))
    ));
    runtime.shutdown().await?;
    assert_eq!(executor.running.load(Ordering::SeqCst), 0);
    Ok(())
}

#[tokio::test]
async fn event_sink_acknowledges_persistence_and_requires_execution_stopped() -> Result<()> {
    let fixture = Fixture::new().await?;
    let turn = exoharness::test_support::begin_test_turn(fixture.thread.as_ref()).await?;
    let key = HarnessTurnKey {
        agent_id: fixture.agent.record().id,
        thread_id: fixture.thread.record().id,
        turn_id: turn.record().id,
    };
    let events = HarnessEvents::default();
    let mut completion = events.register(
        fixture.agent.record().id,
        Arc::clone(&fixture.thread),
        turn,
        None,
    )?;
    let ack = events
        .emit(HarnessEvent::TurnEvents {
            key,
            events: vec![EventData::Custom {
                event_type: "test".to_string(),
                payload: serde_json::json!({ "value": 42 }),
            }],
        })
        .await?;
    assert_eq!(ack.events.len(), 1);
    assert!(fixture.thread.get_event(ack.events[0].id).await?.is_some());
    let terminal_ack = events
        .emit(HarnessEvent::TurnFinished {
            key,
            outcome: HarnessTurnOutcome::Completed(None),
            events: vec![EventData::Custom {
                event_type: "terminal".to_string(),
                payload: serde_json::json!({ "done": true }),
            }],
        })
        .await?;
    assert_eq!(terminal_ack.events.len(), 1);
    assert!(
        fixture
            .thread
            .get_event(terminal_ack.events[0].id)
            .await?
            .is_some()
    );
    assert!(completion.try_recv().is_err());
    assert!(
        events
            .emit(HarnessEvent::TurnEvents {
                key,
                events: Vec::new()
            })
            .await
            .is_err()
    );
    assert!(
        events
            .emit(HarnessEvent::TurnFinished {
                key,
                outcome: HarnessTurnOutcome::Completed(None),
                events: Vec::new()
            })
            .await
            .is_err()
    );
    events.emit(HarnessEvent::ExecutionStopped { key }).await?;
    assert!(matches!(
        completion.await?,
        HarnessTurnOutcome::Completed(None)
    ));
    assert!(
        events
            .emit(HarnessEvent::ExecutionStopped { key })
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn harness_turn_publishes_canonical_tools_after_persistence() -> Result<()> {
    let fixture = Fixture::new().await?;
    let turn = exoharness::test_support::begin_test_turn(fixture.thread.as_ref()).await?;
    let key = HarnessTurnKey::new(
        fixture.agent.record().id,
        fixture.thread.record().id,
        turn.record().id,
    );
    let events = Arc::new(HarnessEvents::default());
    let (stream, mut receiver) = mpsc::unbounded_channel();
    let completion = events.register(
        fixture.agent.record().id,
        fixture.thread.clone(),
        turn.clone(),
        Some(stream),
    )?;
    let turn = HarnessTurn::new(turn, HarnessEventSink::new(events.clone()), key);
    let result = turn
        .add_events(vec![
            EventData::ToolRequested {
                tool_call_id: "call".into(),
                response_id: None,
                request: exoharness::ToolRequest {
                    function_name: "shell".into(),
                    arguments: Default::default(),
                    namespace: None,
                },
            },
            EventData::ToolResult {
                tool_call_id: "call".into(),
                result: serde_json::json!({ "ok": true }),
            },
        ])
        .await?;
    for id in &result.event_ids {
        assert!(fixture.thread.get_event(*id).await?.is_some());
    }
    assert!(
        matches!(receiver.try_recv()??, crate::ExecutionStreamEvent::ToolCall { tool_call_id, .. } if tool_call_id == "call")
    );
    assert!(
        matches!(receiver.try_recv()??, crate::ExecutionStreamEvent::ToolResult { tool_call_id, .. } if tool_call_id == "call")
    );
    assert!(receiver.try_recv().is_err());
    events
        .emit(HarnessEvent::TurnFinished {
            key,
            outcome: HarnessTurnOutcome::Completed(None),
            events: vec![],
        })
        .await?;
    assert!(
        turn.add_events(vec![EventData::ToolResult {
            tool_call_id: "late".into(),
            result: serde_json::json!({})
        }])
        .await
        .is_err()
    );
    events.emit(HarnessEvent::ExecutionStopped { key }).await?;
    assert!(matches!(
        completion.await?,
        HarnessTurnOutcome::Completed(None)
    ));
    Ok(())
}
