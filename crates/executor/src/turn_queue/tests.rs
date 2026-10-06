use super::*;
use crate::{AgentConfig, ConversationConfig, ExecutorStreamMode, HarnessExecutor};
use async_trait::async_trait;
use exoharness::{BasicExoHarness, ExoHarness, TurnHandle};
use std::time::Duration;
use tempfile::TempDir;
use tokio::sync::{Semaphore, mpsc};

struct Controlled {
    started: mpsc::UnboundedSender<exoharness::TurnId>,
    release: Arc<Semaphore>,
}

#[async_trait]
impl HarnessExecutor for Controlled {
    fn name(&self) -> &'static str {
        "queue-test"
    }
    async fn execute_turn(
        &self,
        _: &dyn AgentHandle,
        _: Arc<dyn ThreadHandle>,
        turn: Arc<dyn TurnHandle>,
        _: &AgentConfig,
        _: &ConversationConfig,
        _: &SendRequest,
        _: ExecutorStreamMode<'_>,
        _: Option<&dyn crate::execution_tracing::TurnExecutionTrace>,
    ) -> Result<()> {
        self.started.send(turn.record().id)?;
        self.release.acquire().await?.forget();
        Ok(())
    }
}

struct Fixture {
    temp: TempDir,
    state: Arc<BasicExoHarness>,
    runtime: Runtime,
    agent: Arc<dyn AgentHandle>,
    thread: Arc<dyn ThreadHandle>,
    release: Arc<Semaphore>,
    started: mpsc::UnboundedReceiver<exoharness::TurnId>,
}

async fn fixture() -> Result<Fixture> {
    let temp = TempDir::new()?;
    let state =
        Arc::new(BasicExoHarness::new(crate::test_support::local_test_config(temp.path())).await?);
    let queue = state.turn_coordinator();
    let (started, received) = mpsc::unbounded_channel();
    let release = Arc::new(Semaphore::new(0));
    let runtime = Runtime::new(
        LocalProvider::new(
            state.clone(),
            Arc::new(Controlled {
                started,
                release: release.clone(),
            }),
        )
        .with_turn_coordinator(queue.clone(), Some(queue)),
        None,
    );
    let agent = runtime
        .create_agent(crate::test_support::agent_request(
            "queue",
            crate::AgentHarnessKind::Basic,
        ))
        .await?;
    let thread = agent.new_thread(Default::default()).await?;
    Ok(Fixture {
        temp,
        state,
        runtime,
        agent,
        thread,
        release,
        started: received,
    })
}

fn request() -> SendRequest {
    SendRequest {
        input: vec![crate::harness_helpers::user_message("hello")],
        session_id: None,
    }
}

async fn finish(mut stream: ExecutionStreamHandle) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(event) = stream.next().await {
            event?;
        }
        Ok(())
    })
    .await?
}

#[tokio::test]
async fn accepts_while_running_and_dropped_observers_do_not_cancel_work() -> Result<()> {
    let Fixture {
        temp: _temp,
        state: _state,
        runtime,
        agent,
        thread,
        release,
        mut started,
    } = fixture().await?;
    let (first, first_stream) = runtime
        .start_turn(agent.clone(), thread.clone(), request(), false, None)
        .await?;
    assert_eq!(started.recv().await, Some(first.id));
    let (second, second_stream) = tokio::time::timeout(
        Duration::from_secs(1),
        runtime.start_turn(agent, thread.clone(), request(), false, None),
    )
    .await??;
    assert_ne!(first.id, second.id);
    assert!(started.try_recv().is_err());
    drop(second_stream);
    release.add_permits(1);
    finish(first_stream).await?;
    assert_eq!(started.recv().await, Some(second.id));
    release.add_permits(1);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if matches!(
                journal_status(thread.as_ref(), &second).await?,
                JournalStatus::Finished(_)
            ) {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::task::yield_now().await;
        }
    })
    .await??;
    runtime.shutdown().await
}

#[tokio::test]
async fn queued_cancellation_skips_execution_and_interrupt_starts_its_replacement() -> Result<()> {
    let Fixture {
        temp: _temp,
        state: _state,
        runtime,
        agent,
        thread,
        release,
        mut started,
    } = fixture().await?;
    let (first, first_stream) = runtime
        .start_turn(agent.clone(), thread.clone(), request(), false, None)
        .await?;
    assert_eq!(started.recv().await, Some(first.id));
    let (cancelled, cancelled_stream) = runtime
        .start_turn(agent.clone(), thread.clone(), request(), false, None)
        .await?;
    assert!(
        runtime
            .cancel_turn(HarnessTurnKey::new(thread.record().id, cancelled.id))
            .await?
    );
    let (replacement, replacement_stream) = runtime
        .start_turn_with_options(
            agent,
            thread.clone(),
            request(),
            false,
            None,
            TurnOptions {
                attention: TurnAttention::Interrupt,
                idempotency_key: Some("replacement".into()),
            },
        )
        .await?;
    assert!(
        finish(first_stream)
            .await
            .unwrap_err()
            .to_string()
            .contains("cancel")
    );
    assert!(
        finish(cancelled_stream)
            .await
            .unwrap_err()
            .to_string()
            .contains("cancel")
    );
    assert_eq!(started.recv().await, Some(replacement.id));
    release.add_permits(1);
    finish(replacement_stream).await?;
    assert!(started.try_recv().is_err());
    runtime.shutdown().await
}

#[tokio::test]
async fn restart_reconciles_completion_and_admission_before_journal_creation() -> Result<()> {
    let Fixture {
        temp,
        state,
        runtime,
        agent,
        thread,
        release: _release,
        started: _started,
    } = fixture().await?;
    let queue = state.turn_coordinator::<TurnWork>();
    let scope = TurnThread {
        agent_id: agent.record().id,
        thread_id: thread.record().id,
    };
    let work = TurnWork {
        streaming: false,
        agent_config: runtime.get_agent_config(agent.as_ref()).await?,
        thread_config: Default::default(),
        request: request(),
    };
    let entry = |started| QueuedTurn {
        turn: TurnRecord {
            id: Uuid7::now(),
            session_id: Uuid7::now(),
        },
        work: work.clone(),
        principal: None,
        idempotency_key: Some(Uuid7::now().to_string()),
        attention: Default::default(),
        started,
        cancelled: false,
    };
    let completed = entry(true);
    queue.enqueue(scope, completed.clone()).await?;
    let turn = thread
        .begin_turn(exoharness::BeginTurnRequest {
            turn: Some(completed.turn.clone()),
            initial_events: vec![work.event()?],
            ..Default::default()
        })
        .await?;
    turn.finish().await?;
    let admitted = entry(true); // Persisted admission, but no journal was written.
    let cancelled = entry(true);
    queue.enqueue(scope, cancelled.clone()).await?;
    let cancelled_turn = thread
        .begin_turn(exoharness::BeginTurnRequest {
            turn: Some(cancelled.turn.clone()),
            initial_events: vec![work.event()?],
            ..Default::default()
        })
        .await?;
    queue
        .cancel(scope, cancelled.turn.id, CancelAuthority::ThreadOwner)
        .await?;
    drop(cancelled_turn);
    queue.enqueue(scope, admitted.clone()).await?;
    let pending = entry(false);
    queue.enqueue(scope, pending.clone()).await?;
    drop(queue);
    drop(turn);
    drop(thread);
    drop(agent);
    drop(runtime);
    drop(state);

    let state =
        Arc::new(BasicExoHarness::new(crate::test_support::local_test_config(temp.path())).await?);
    let queue = state.turn_coordinator();
    let (started, mut received) = mpsc::unbounded_channel();
    let release = Arc::new(Semaphore::new(2));
    let runtime = Runtime::new(
        LocalProvider::new(state.clone(), Arc::new(Controlled { started, release }))
            .with_turn_coordinator(queue.clone(), Some(queue.clone())),
        None,
    );
    runtime.recover_unfinished_turns().await?;
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), received.recv()).await?,
        Some(admitted.turn.id)
    );
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), received.recv()).await?,
        Some(pending.turn.id)
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        while !queue.pending_threads().await?.is_empty() {
            tokio::task::yield_now().await;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    assert!(received.try_recv().is_err());
    let agent = state.get_agent(&scope.agent_id).await?.unwrap();
    let thread = agent.get_thread(&scope.thread_id).await?.unwrap();
    assert!(matches!(
        journal_status(thread.as_ref(), &cancelled.turn).await?,
        JournalStatus::Finished(Err(_))
    ));
    let duplicate = queue.enqueue(scope, completed.clone()).await?;
    assert!(duplicate.duplicate);
    assert_eq!(duplicate.turn, completed.turn);
    runtime.shutdown().await
}
