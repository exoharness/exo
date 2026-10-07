use super::*;
use crate::{AgentConfig, ConversationConfig, ExecutorStreamMode, HarnessExecutor};
use async_trait::async_trait;
use exoharness::turn_coordinator::TurnAdmission;
use exoharness::turn_coordinator::TurnAttention;
use exoharness::{BasicExoHarness, EventKind, ExoHarness, TurnHandle};
use std::{path::Path, time::Duration};
use tempfile::TempDir;
use tokio::sync::{Semaphore, mpsc, oneshot};

struct Cleanup {
    started: oneshot::Sender<()>,
    release: oneshot::Receiver<()>,
}

struct Controlled {
    started: mpsc::UnboundedSender<exoharness::TurnId>,
    release: Semaphore,
    resumed: Mutex<Vec<(String, bool)>>,
    tool_round: AtomicBool,
    cleanup: Mutex<Option<Cleanup>>,
}

#[async_trait]
impl HarnessExecutor for Controlled {
    fn name(&self) -> &'static str {
        "queue-test"
    }

    fn can_suspend_turn(&self, config: &AgentConfig) -> bool {
        config.harness == crate::AgentHarnessKind::Basic
    }

    async fn cancel_turn(&self, _: &dyn ThreadHandle, _: &AgentConfig) -> Result<()> {
        let cleanup = self.cleanup.lock().unwrap().take();
        if let Some(cleanup) = cleanup {
            cleanup
                .started
                .send(())
                .map_err(|_| anyhow!("cleanup observer disconnected"))?;
            cleanup.release.await?;
        }
        Ok(())
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
        let tool_round = self.tool_round.swap(false, Ordering::SeqCst);
        if tool_round {
            turn.add_events(vec![EventData::ToolRequested {
                tool_call_id: "in-flight".into(),
                response_id: None,
                request: exoharness::ToolRequest {
                    namespace: None,
                    function_name: "side_effect".into(),
                    arguments: Default::default(),
                },
            }])
            .await?;
        }
        self.started.send(turn.record().id)?;
        self.release.acquire().await?.forget();
        if tool_round {
            turn.add_events(vec![EventData::ToolResult {
                tool_call_id: "in-flight".into(),
                result: serde_json::Value::Null,
            }])
            .await?;
            // Give a pending suspension a chance to win at this safe boundary.
            tokio::task::yield_now().await;
            self.release.acquire().await?.forget();
        }
        Ok(())
    }

    async fn resume_turn(
        &self,
        _: &dyn AgentHandle,
        _: Arc<dyn ThreadHandle>,
        turn: Arc<dyn TurnHandle>,
        config: &AgentConfig,
        _: &ConversationConfig,
        _: &SendRequest,
        stream: ExecutorStreamMode<'_>,
        _: Option<&dyn crate::execution_tracing::TurnExecutionTrace>,
    ) -> Result<()> {
        self.resumed.lock().unwrap().push((
            config.model.clone(),
            matches!(stream, ExecutorStreamMode::Enabled(_)),
        ));
        self.started.send(turn.record().id)?;
        self.release.acquire().await?.forget();
        Ok(())
    }
}

struct Fixture {
    state: Arc<BasicExoHarness>,
    runtime: Runtime,
    agent: Arc<dyn AgentHandle>,
    thread: Arc<dyn ThreadHandle>,
    executor: Arc<Controlled>,
    started: mpsc::UnboundedReceiver<exoharness::TurnId>,
}

async fn fixture() -> Result<(TempDir, Fixture)> {
    let temp = TempDir::new()?;
    let fixture = Fixture::open(temp.path(), None).await?;
    Ok((temp, fixture))
}

impl Fixture {
    async fn open(root: &Path, scope: Option<TurnThread>) -> Result<Self> {
        let state =
            Arc::new(BasicExoHarness::new(crate::test_support::local_test_config(root)).await?);
        let queue = state.turn_coordinator();
        let (sender, started) = mpsc::unbounded_channel();
        let executor = Arc::new(Controlled {
            started: sender,
            release: Semaphore::new(0),
            resumed: Mutex::default(),
            tool_round: AtomicBool::new(false),
            cleanup: Mutex::default(),
        });
        let runtime = Runtime::new(
            LocalProvider::new(state.clone(), executor.clone()).with_turn_coordinator(queue),
            None,
        );
        let (agent, thread) = match scope {
            Some(scope) => {
                let agent = state.get_agent(&scope.agent_id).await?.unwrap();
                let thread = agent.get_thread(&scope.thread_id).await?.unwrap();
                (agent, thread)
            }
            None => {
                let agent = runtime
                    .create_agent(crate::test_support::agent_request(
                        "queue",
                        crate::AgentHarnessKind::Basic,
                    ))
                    .await?;
                let thread = agent.new_thread(Default::default()).await?;
                (agent, thread)
            }
        };
        Ok(Self {
            state,
            runtime,
            agent,
            thread,
            executor,
            started,
        })
    }

    fn scope(&self) -> TurnThread {
        TurnThread {
            agent_id: self.agent.record().id,
            thread_id: self.thread.record().id,
        }
    }

    fn key(&self, turn: &TurnRecord) -> HarnessTurnKey {
        HarnessTurnKey::new(self.agent.record().id, self.thread.record().id, turn.id)
    }

    async fn start(&self) -> Result<(TurnRecord, ExecutionStreamHandle)> {
        self.runtime
            .start_turn(
                self.agent.clone(),
                self.thread.clone(),
                request(),
                false,
                None,
                Default::default(),
            )
            .await
    }

    async fn started(&mut self, turn: exoharness::TurnId) -> Result<()> {
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), self.started.recv()).await?,
            Some(turn)
        );
        Ok(())
    }

    async fn submission(&self) -> Result<TurnSubmission<TurnWork>> {
        Ok(TurnSubmission {
            turn: TurnRecord {
                id: Uuid7::now(),
                session_id: Uuid7::now(),
            },
            work: TurnWork {
                streaming: false,
                agent_config: self.runtime.get_agent_config(self.agent.as_ref()).await?,
                thread_config: Default::default(),
                request: request(),
            },
            principal: None,
            options: TurnOptions {
                idempotency_key: Some(Uuid7::now().to_string()),
                attention: TurnAttention::Wake,
            },
        })
    }

    async fn journal(&self, turn: &TurnRecord) -> Result<Vec<EventKind>> {
        Ok(self
            .thread
            .get_events(Some(EventQuery {
                turn_id: Some(turn.id),
                direction: Some(EventQueryDirection::Asc),
                types: Some(vec![
                    EventKind::TURN_STARTED,
                    EventKind::ERROR,
                    EventKind::TURN_ENDED,
                    EventKind::TOOL_REQUESTED,
                    EventKind::TOOL_RESULT,
                ]),
                ..Default::default()
            }))
            .await?
            .events
            .into_iter()
            .map(|event| event.data.kind())
            .collect())
    }

    async fn wait_idle(&self) -> Result<()> {
        let queue = self.state.turn_coordinator::<TurnWork>();
        tokio::time::timeout(Duration::from_secs(5), async {
            while !queue.pending_threads().await?.is_empty() {
                tokio::task::yield_now().await;
            }
            Ok::<_, anyhow::Error>(())
        })
        .await?
    }
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
async fn continued_turns_do_not_restart_implicit_or_explicit_sessions() -> Result<()> {
    for explicit_session in [false, true] {
        let (_temp, mut f) = fixture().await?;
        let session_id = if explicit_session {
            Some(f.thread.start_session().await?)
        } else {
            None
        };
        let (first, stream) = f
            .runtime
            .start_turn(
                f.agent.clone(),
                f.thread.clone(),
                SendRequest {
                    session_id,
                    ..request()
                },
                false,
                None,
                Default::default(),
            )
            .await?;
        if let Some(session_id) = session_id {
            assert_eq!(first.session_id, session_id);
        }
        f.started(first.id).await?;
        f.executor.release.add_permits(1);
        finish(stream).await?;

        let continuing = SendRequest {
            session_id: Some(first.session_id),
            ..request()
        };
        let (second, stream) = f
            .runtime
            .start_turn(
                f.agent.clone(),
                f.thread.clone(),
                continuing.clone(),
                false,
                None,
                Default::default(),
            )
            .await?;
        assert_eq!(second.session_id, first.session_id);
        f.started(second.id).await?;
        let (cancelled, cancelled_stream) = f
            .runtime
            .start_turn(
                f.agent.clone(),
                f.thread.clone(),
                continuing,
                false,
                None,
                Default::default(),
            )
            .await?;
        assert_eq!(cancelled.session_id, first.session_id);
        assert!(f.runtime.cancel_turn(f.key(&cancelled)).await?);
        f.executor.release.add_permits(1);
        finish(stream).await?;
        assert!(
            finish(cancelled_stream)
                .await
                .unwrap_err()
                .to_string()
                .contains("cancel")
        );
        f.wait_idle().await?;
        assert!(f.started.try_recv().is_err());

        let events = f
            .thread
            .get_events(Some(EventQuery {
                session_id: Some(first.session_id),
                types: Some(vec![EventKind::SESSION_STARTED, EventKind::TURN_STARTED]),
                direction: Some(EventQueryDirection::Asc),
                ..Default::default()
            }))
            .await?
            .events;
        assert_eq!(
            events
                .iter()
                .map(|event| event.data.kind())
                .collect::<Vec<_>>(),
            vec![
                EventKind::SESSION_STARTED,
                EventKind::TURN_STARTED,
                EventKind::TURN_STARTED,
                EventKind::TURN_STARTED,
            ],
        );
        f.runtime.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
async fn accepts_while_running_and_dropped_observers_do_not_cancel_work() -> Result<()> {
    let (_temp, mut f) = fixture().await?;
    let (first, stream) = f.start().await?;
    f.started(first.id).await?;
    let (second, second_stream) = tokio::time::timeout(Duration::from_secs(1), f.start()).await??;
    assert_ne!(first.id, second.id);
    assert!(f.started.try_recv().is_err());
    drop(second_stream);
    f.executor.release.add_permits(1);
    finish(stream).await?;
    f.started(second.id).await?;
    f.executor.release.add_permits(1);
    f.wait_idle().await?;
    assert!(matches!(
        journal_status(f.thread.as_ref(), &second).await?,
        JournalStatus::Finished(Ok(_))
    ));
    f.runtime.shutdown().await
}

#[tokio::test]
async fn shutdown_preserves_explicit_cancellation() -> Result<()> {
    let (_temp, mut f) = fixture().await?;
    f.runtime.shutdown().await?;
    let provider = LocalProvider::new(f.state.clone(), f.executor.clone())
        .with_turn_coordinator(f.state.turn_coordinator());
    f.runtime = Runtime::new(provider.clone(), None);
    let (turn, stream) = f.start().await?;
    f.started(turn.id).await?;
    provider
        .harness
        .submit(HarnessCommand::CancelTurn { key: f.key(&turn) })
        .await?;
    provider.harness.shutdown().await?;
    assert!(
        finish(stream)
            .await
            .unwrap_err()
            .to_string()
            .contains("cancel")
    );
    assert!(matches!(
        journal_status(f.thread.as_ref(), &turn).await?,
        JournalStatus::Finished(Err(error)) if error.to_string().contains("cancel")
    ));
    f.runtime.shutdown().await
}

#[tokio::test]
async fn queued_cancellation_skips_execution_and_interrupt_starts_its_replacement() -> Result<()> {
    let (_temp, mut f) = fixture().await?;
    let (first, first_stream) = f.start().await?;
    f.started(first.id).await?;
    let (cancelled, cancelled_stream) = f.start().await?;
    assert!(f.runtime.cancel_turn(f.key(&cancelled)).await?);
    let (replacement, stream) = f
        .runtime
        .start_turn(
            f.agent.clone(),
            f.thread.clone(),
            request(),
            false,
            None,
            TurnOptions {
                attention: TurnAttention::Interrupt,
                idempotency_key: Some("replacement".into()),
            },
        )
        .await?;
    for stream in [first_stream, cancelled_stream] {
        assert!(
            finish(stream)
                .await
                .unwrap_err()
                .to_string()
                .contains("cancel")
        );
    }
    f.started(replacement.id).await?;
    f.executor.release.add_permits(1);
    finish(stream).await?;
    assert!(f.started.try_recv().is_err());
    f.runtime.shutdown().await
}

#[tokio::test]
async fn restart_reconciles_completion_and_admission_before_journal_creation() -> Result<()> {
    let (temp, f) = fixture().await?;
    let scope = f.scope();
    let queue = f.state.turn_coordinator();
    let completed = f.submission().await?;
    queue.enqueue(scope, completed.clone()).await?;
    let lease = queue.claim(scope).await?.unwrap();
    queue.start(&lease, completed.turn.id).await?;
    drop(lease);
    let turn = f
        .thread
        .begin_turn(exoharness::BeginTurnRequest {
            turn: completed.turn.clone(),
            new_session: completed.work.request.session_id.is_none(),
            input: Vec::new(),
            initial_events: vec![completed.work.event()?],
        })
        .await?;
    turn.finish().await?;
    drop(turn);
    let cancelled = f.submission().await?;
    queue.enqueue(scope, cancelled.clone()).await?;
    let turn = f
        .thread
        .begin_turn(exoharness::BeginTurnRequest {
            turn: cancelled.turn.clone(),
            new_session: cancelled.work.request.session_id.is_none(),
            input: Vec::new(),
            initial_events: vec![cancelled.work.event()?],
        })
        .await?;
    drop(turn);
    queue
        .cancel(scope, cancelled.turn.id, TurnAuthority::ThreadOwner)
        .await?;
    let admitted = f.submission().await?; // Admission persisted before its journal.
    queue.enqueue(scope, admitted.clone()).await?;
    let pending = f.submission().await?;
    queue.enqueue(scope, pending.clone()).await?;
    f.runtime.shutdown().await?;
    drop(queue);
    drop(f);

    let mut f = Fixture::open(temp.path(), Some(scope)).await?;
    f.executor.release.add_permits(2);
    f.runtime.recover_unfinished_turns().await?;
    f.started(admitted.turn.id).await?;
    f.started(pending.turn.id).await?;
    f.wait_idle().await?;
    assert!(f.started.try_recv().is_err());
    assert!(matches!(
        journal_status(f.thread.as_ref(), &cancelled.turn).await?,
        JournalStatus::Finished(Err(_))
    ));
    let duplicate = f
        .state
        .turn_coordinator()
        .enqueue(scope, completed.clone())
        .await?;
    assert!(duplicate.duplicate);
    assert_eq!(duplicate.turn, completed.turn);
    f.runtime.shutdown().await
}

#[tokio::test]
async fn recovery_uses_prepared_work_instead_of_the_acceptance_snapshot() -> Result<()> {
    let (_temp, mut f) = fixture().await?;
    let submission = f.submission().await?;
    let queue = f.state.turn_coordinator();
    queue.enqueue(f.scope(), submission.clone()).await?;
    let lease = queue.claim(f.scope()).await?.unwrap();
    queue.start(&lease, submission.turn.id).await?;
    drop(lease);
    let mut prepared = submission.work;
    prepared.agent_config.model = "prepared-model".into();
    prepared.streaming = true;
    let turn = f
        .thread
        .begin_turn(exoharness::BeginTurnRequest {
            turn: submission.turn.clone(),
            new_session: prepared.request.session_id.is_none(),
            input: Vec::new(),
            initial_events: vec![prepared.event()?],
        })
        .await?;
    drop(turn);
    f.runtime.recover_unfinished_turns().await?;
    f.started(submission.turn.id).await?;
    assert_eq!(
        *f.executor.resumed.lock().unwrap(),
        vec![("prepared-model".into(), true)]
    );
    f.executor.release.add_permits(1);
    f.wait_idle().await?;
    assert!(matches!(
        journal_status(f.thread.as_ref(), &submission.turn).await?,
        JournalStatus::Finished(Ok(_))
    ));
    f.runtime.shutdown().await
}

#[tokio::test]
async fn restart_after_queue_start_before_journal_preserves_the_accepted_identity() -> Result<()> {
    let (temp, f) = fixture().await?;
    let scope = f.scope();
    let submission = f.submission().await?;
    let queue = f.state.turn_coordinator();
    queue.enqueue(scope, submission.clone()).await?;
    let lease = queue.claim(scope).await?.unwrap();
    queue.start(&lease, submission.turn.id).await?;
    drop(lease);
    f.runtime.shutdown().await?;
    drop(queue);
    drop(f);

    let mut f = Fixture::open(temp.path(), Some(scope)).await?;
    f.runtime.recover_unfinished_turns().await?;
    f.started(submission.turn.id).await?;
    assert!(f.executor.resumed.lock().unwrap().is_empty());
    f.executor.release.add_permits(1);
    f.wait_idle().await?;
    assert_eq!(
        f.journal(&submission.turn).await?,
        vec![EventKind::TURN_STARTED, EventKind::TURN_ENDED]
    );
    f.runtime.shutdown().await
}

#[tokio::test]
async fn default_queue_does_not_recover_work_from_the_journal_after_restart() -> Result<()> {
    let (_temp, mut f) = fixture().await?;
    f.runtime.shutdown().await?;
    f.runtime = Runtime::new(
        LocalProvider::new(f.state.clone(), f.executor.clone()),
        None,
    );
    let (old, old_stream) = f.start().await?;
    f.started(old.id).await?;
    f.runtime.shutdown().await?;
    drop(old_stream);

    f.runtime = Runtime::new(
        LocalProvider::new(f.state.clone(), f.executor.clone()),
        None,
    );
    f.runtime.recover_unfinished_turns().await?;
    assert!(f.started.try_recv().is_err());
    assert!(f.executor.resumed.lock().unwrap().is_empty());
    assert_eq!(f.journal(&old).await?, vec![EventKind::TURN_STARTED]);
    let (new, stream) = f.start().await?;
    f.started(new.id).await?;
    f.executor.release.add_permits(1);
    finish(stream).await?;
    assert_eq!(f.journal(&old).await?, vec![EventKind::TURN_STARTED]);
    f.runtime.shutdown().await
}

#[tokio::test]
async fn suspension_survives_restart_and_resumes_the_same_turn_before_later_work() -> Result<()> {
    let (temp, mut f) = fixture().await?;
    let scope = f.scope();
    let (first, mut first_stream) = f.start().await?;
    f.started(first.id).await?;
    let (second, second_stream) = f.start().await?;
    let key = f.key(&first);
    assert!(f.runtime.suspend_turn(key).await?);
    let paused = tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(event) = first_stream.next().await {
            if let ExecutionStreamEvent::Suspended(turn) = event? {
                return Ok::<_, anyhow::Error>(turn);
            }
        }
        anyhow::bail!("suspension was not reported")
    })
    .await??;
    assert_eq!(paused, first);
    assert_eq!(f.journal(&first).await?, vec![EventKind::TURN_STARTED]);
    assert!(f.started.try_recv().is_err());
    // Wait for the paused drain to release ownership before counting events.
    let queue = f.state.turn_coordinator::<TurnWork>();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(lease) = queue.claim(scope).await? {
                assert!(queue.release_if_idle(&lease).await?);
                return Ok::<_, anyhow::Error>(());
            }
            tokio::task::yield_now().await;
        }
    })
    .await??;
    drop(queue);
    f.runtime.shutdown().await?;
    let mut suspensions = 1;
    while let Some(Ok(event)) = first_stream.next().await {
        suspensions += usize::from(matches!(event, ExecutionStreamEvent::Suspended(_)));
    }
    assert_eq!(suspensions, 1);
    drop(second_stream);
    drop(first_stream);
    drop(f);

    let mut f = Fixture::open(temp.path(), Some(scope)).await?;
    f.runtime.recover_unfinished_turns().await?;
    assert!(f.started.try_recv().is_err());
    let stream = f.runtime.resume_turn(key).await?;
    f.started(first.id).await?;
    f.executor.release.add_permits(1);
    finish(stream).await?;
    f.started(second.id).await?;
    f.executor.release.add_permits(1);
    f.wait_idle().await?;
    assert_eq!(
        f.journal(&first).await?,
        vec![EventKind::TURN_STARTED, EventKind::TURN_ENDED]
    );
    f.runtime.shutdown().await
}

#[tokio::test]
async fn remote_control_changes_reach_an_executing_turn() -> Result<()> {
    let (_temp, mut f) = fixture().await?;
    let (first, first_stream) = f.start().await?;
    f.started(first.id).await?;
    let (second, second_stream) = f.start().await?;
    f.state
        .turn_coordinator::<TurnWork>()
        .cancel(f.scope(), first.id, TurnAuthority::ThreadOwner)
        .await?;
    assert!(
        finish(first_stream)
            .await
            .unwrap_err()
            .to_string()
            .contains("cancel")
    );
    f.started(second.id).await?;
    f.executor.release.add_permits(1);
    finish(second_stream).await?;
    f.runtime.shutdown().await
}

#[tokio::test]
async fn unrecoverable_head_is_ended_without_failing_the_next_caller() -> Result<()> {
    let (_temp, mut f) = fixture().await?;
    let mut head = f.submission().await?;
    head.principal = Some("missing-caller".into());
    f.state
        .turn_coordinator()
        .enqueue(f.scope(), head.clone())
        .await?;
    let (next, stream) = f.start().await?;
    f.started(next.id).await?;
    assert!(matches!(
        journal_status(f.thread.as_ref(), &head.turn).await?,
        JournalStatus::Finished(Err(_))
    ));
    f.executor.release.add_permits(1);
    finish(stream).await?;
    f.runtime.shutdown().await
}

#[tokio::test]
async fn rejected_submission_is_finalized_exactly_once() -> Result<()> {
    let (_temp, f) = fixture().await?;
    f.runtime.shutdown().await?;
    let queue = f.state.turn_coordinator();
    let provider =
        LocalProvider::new(f.state.clone(), f.executor.clone()).with_turn_coordinator(queue);
    provider.harness.shutdown().await?;
    let runtime = Runtime::new(provider, None);
    let (turn, stream) = runtime
        .start_turn(
            f.agent.clone(),
            f.thread.clone(),
            request(),
            false,
            None,
            Default::default(),
        )
        .await?;
    assert!(finish(stream).await.is_err());
    assert_eq!(
        f.journal(&turn).await?,
        vec![
            EventKind::TURN_STARTED,
            EventKind::ERROR,
            EventKind::TURN_ENDED
        ]
    );
    runtime.shutdown().await
}

enum QueueTestFault {
    HoldAcceptance {
        count: std::sync::atomic::AtomicUsize,
        published: mpsc::UnboundedSender<exoharness::TurnId>,
        release: Arc<Semaphore>,
    },
    CancelOnStart,
    WatchFailure {
        once: AtomicBool,
        release: Arc<Semaphore>,
    },
}

struct TestCoordinator {
    inner: Arc<dyn TurnQueue<TurnWork>>,
    fault: QueueTestFault,
}

#[async_trait]
impl exoharness::turn_coordinator::TurnAdmission<TurnWork> for TestCoordinator {
    async fn enqueue(
        &self,
        thread: TurnThread,
        turn: TurnSubmission<TurnWork>,
    ) -> Result<exoharness::turn_coordinator::AcceptedTurn> {
        let accepted = self.inner.enqueue(thread, turn).await?;
        if let QueueTestFault::HoldAcceptance {
            count,
            published,
            release,
        } = &self.fault
            && count.fetch_add(1, Ordering::SeqCst) == 1
        {
            published.send(accepted.turn.id)?;
            release.acquire().await?.forget();
        }
        Ok(accepted)
    }
    async fn cancel(
        &self,
        thread: TurnThread,
        turn: exoharness::TurnId,
        authority: TurnAuthority,
    ) -> Result<TurnControlOutcome> {
        self.inner.cancel(thread, turn, authority).await
    }
}

#[async_trait]
impl TurnQueue<TurnWork> for TestCoordinator {
    async fn pending_threads(&self) -> Result<Vec<TurnThread>> {
        self.inner.pending_threads().await
    }
    async fn claim(&self, thread: TurnThread) -> Result<Option<TurnLease>> {
        self.inner.claim(thread).await
    }
    async fn get(
        &self,
        thread: TurnThread,
        turn: exoharness::TurnId,
    ) -> Result<Option<QueuedTurn<TurnWork>>> {
        self.inner.get(thread, turn).await
    }
    async fn peek(&self, lease: &TurnLease) -> Result<Option<QueuedTurn<TurnWork>>> {
        self.inner.peek(lease).await
    }
    async fn start(
        &self,
        lease: &TurnLease,
        turn: exoharness::TurnId,
    ) -> Result<exoharness::turn_coordinator::TurnStart<TurnWork>> {
        let mut started = self.inner.start(lease, turn).await?;
        match &self.fault {
            QueueTestFault::CancelOnStart => {
                self.inner
                    .cancel(lease.thread, turn, TurnAuthority::ThreadOwner)
                    .await?;
            }
            QueueTestFault::WatchFailure { once, release }
                if once.swap(false, Ordering::SeqCst) =>
            {
                let release = release.clone();
                started.control =
                    Box::pin(futures::stream::once(async { Ok(TurnControl::Run) }).chain(
                        futures::stream::once(async move {
                            release.acquire().await?.forget();
                            Err(anyhow!("control storage unavailable"))
                        }),
                    ));
            }
            _ => {}
        }
        Ok(started)
    }
    async fn set_suspended(
        &self,
        thread: TurnThread,
        turn: exoharness::TurnId,
        authority: TurnAuthority,
        suspended: bool,
    ) -> Result<TurnControlOutcome> {
        self.inner
            .set_suspended(thread, turn, authority, suspended)
            .await
    }
    async fn acknowledge(
        &self,
        lease: &TurnLease,
        turn: exoharness::TurnId,
    ) -> Result<Option<QueuedTurn<TurnWork>>> {
        self.inner.acknowledge(lease, turn).await
    }
    async fn release_if_idle(&self, lease: &TurnLease) -> Result<bool> {
        self.inner.release_if_idle(lease).await
    }
}

#[tokio::test]
async fn live_drain_can_finish_before_enqueue_returns_without_losing_the_observer() -> Result<()> {
    let (_temp, mut f) = fixture().await?;
    f.runtime.shutdown().await?;
    let queue = f.state.turn_coordinator();
    let (published, mut published_rx) = mpsc::unbounded_channel();
    let acceptance = Arc::new(Semaphore::new(0));
    let held = Arc::new(TestCoordinator {
        inner: queue.clone(),
        fault: QueueTestFault::HoldAcceptance {
            count: Default::default(),
            published,
            release: acceptance.clone(),
        },
    });
    f.runtime = Runtime::new(
        LocalProvider::new(f.state.clone(), f.executor.clone()).with_turn_coordinator(held),
        None,
    );
    let (first, first_stream) = f.start().await?;
    f.started(first.id).await?;
    let (runtime, agent, thread) = (f.runtime.clone(), f.agent.clone(), f.thread.clone());
    let next = tokio::spawn(async move {
        runtime
            .start_turn(agent, thread, request(), false, None, Default::default())
            .await
    });
    let second_id = tokio::time::timeout(Duration::from_secs(5), published_rx.recv())
        .await?
        .unwrap();
    f.executor.release.add_permits(1);
    finish(first_stream).await?;
    f.started(second_id).await?;
    f.executor.release.add_permits(1);
    f.wait_idle().await?;
    acceptance.add_permits(1);
    let (second, stream) = next.await??;
    assert_eq!(second.id, second_id);
    finish(stream).await?;
    f.runtime.shutdown().await
}

#[tokio::test]
async fn immediate_resume_during_suspension_cleanup_keeps_the_turn_unfinished() -> Result<()> {
    let (_temp, mut f) = fixture().await?;
    let (started, stopping) = oneshot::channel();
    let (release, cleanup) = oneshot::channel();
    *f.executor.cleanup.lock().unwrap() = Some(Cleanup {
        started,
        release: cleanup,
    });
    let (turn, original) = f.start().await?;
    f.started(turn.id).await?;
    let key = f.key(&turn);
    f.runtime.suspend_turn(key).await?;
    tokio::time::timeout(Duration::from_secs(5), stopping).await??;
    let resumed = f.runtime.resume_turn(key).await?;
    release.send(()).map_err(|_| anyhow!("cleanup stopped"))?;
    f.started(turn.id).await?;
    f.executor.release.add_permits(1);
    finish(resumed).await?;
    finish(original).await?;
    assert_eq!(
        f.journal(&turn).await?,
        vec![EventKind::TURN_STARTED, EventKind::TURN_ENDED]
    );
    f.runtime.shutdown().await
}

#[tokio::test]
async fn cancellation_during_admission_never_executes_the_turn() -> Result<()> {
    let (_temp, mut f) = fixture().await?;
    f.runtime.shutdown().await?;
    let queue = f.state.turn_coordinator();
    f.runtime = Runtime::new(
        LocalProvider::new(f.state.clone(), f.executor.clone()).with_turn_coordinator(Arc::new(
            TestCoordinator {
                inner: queue.clone(),
                fault: QueueTestFault::CancelOnStart,
            },
        )),
        None,
    );
    let (turn, stream) = f.start().await?;
    assert!(
        finish(stream)
            .await
            .unwrap_err()
            .to_string()
            .contains("cancelled")
    );
    assert!(f.started.try_recv().is_err());
    assert_eq!(
        f.journal(&turn).await?,
        vec![
            EventKind::TURN_STARTED,
            EventKind::ERROR,
            EventKind::TURN_ENDED
        ]
    );
    f.runtime.shutdown().await
}

#[tokio::test]
async fn failed_control_watch_detaches_only_the_head_and_stops_before_recovery() -> Result<()> {
    use futures::FutureExt;
    let (_temp, mut f) = fixture().await?;
    f.runtime.shutdown().await?;
    let queue = f.state.turn_coordinator();
    let failure = Arc::new(Semaphore::new(0));
    f.runtime = Runtime::new(
        LocalProvider::new(f.state.clone(), f.executor.clone()).with_turn_coordinator(Arc::new(
            TestCoordinator {
                inner: queue.clone(),
                fault: QueueTestFault::WatchFailure {
                    once: AtomicBool::new(true),
                    release: failure.clone(),
                },
            },
        )),
        None,
    );
    let (first, first_stream) = f.start().await?;
    f.started(first.id).await?;
    let (second, mut second_stream) = f.start().await?;
    failure.add_permits(1);
    assert!(
        finish(first_stream)
            .await
            .unwrap_err()
            .to_string()
            .contains("work retained for recovery")
    );
    assert!(second_stream.next().now_or_never().is_none());
    assert_eq!(f.journal(&first).await?, vec![EventKind::TURN_STARTED]);
    assert!(f.started.try_recv().is_err());
    let (third, third_stream) = f.start().await?; // Another wake resumes the retained head.
    f.started(first.id).await?;
    f.executor.release.add_permits(1);
    f.started(second.id).await?;
    f.executor.release.add_permits(1);
    finish(second_stream).await?;
    f.started(third.id).await?;
    f.executor.release.add_permits(1);
    finish(third_stream).await?;
    assert_eq!(f.executor.resumed.lock().unwrap().len(), 1);
    f.runtime.shutdown().await
}

#[tokio::test]
async fn suspension_waits_for_tool_results_and_cancellation_can_stop_that_wait() -> Result<()> {
    let (_temp, mut f) = fixture().await?;
    f.executor.tool_round.store(true, Ordering::SeqCst);
    let (turn, mut stream) = f.start().await?;
    f.started(turn.id).await?;
    f.runtime.suspend_turn(f.key(&turn)).await?;
    assert_eq!(
        f.journal(&turn).await?,
        vec![EventKind::TURN_STARTED, EventKind::TOOL_REQUESTED]
    );
    f.executor.release.add_permits(1); // Finish the tool, then wait before completing the turn.
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(event) = stream.next().await {
            if matches!(event?, ExecutionStreamEvent::Suspended(_)) {
                return Ok::<_, anyhow::Error>(());
            }
        }
        anyhow::bail!("turn was not suspended")
    })
    .await??;
    assert_eq!(
        f.journal(&turn).await?,
        vec![
            EventKind::TURN_STARTED,
            EventKind::TOOL_REQUESTED,
            EventKind::TOOL_RESULT
        ]
    );
    let resumed = f.runtime.resume_turn(f.key(&turn)).await?;
    f.started(turn.id).await?;
    f.executor.release.add_permits(1);
    finish(resumed).await?;
    finish(stream).await?;

    f.executor.tool_round.store(true, Ordering::SeqCst);
    let (turn, stream) = f.start().await?;
    f.started(turn.id).await?;
    f.runtime.suspend_turn(f.key(&turn)).await?;
    f.runtime.cancel_turn(f.key(&turn)).await?;
    assert!(
        finish(stream)
            .await
            .unwrap_err()
            .to_string()
            .contains("cancel")
    );
    assert!(matches!(
        journal_status(f.thread.as_ref(), &turn).await?,
        JournalStatus::Finished(Err(_))
    ));
    f.runtime.shutdown().await
}

#[tokio::test]
async fn send_returns_when_its_turn_is_suspended() -> Result<()> {
    let (_temp, mut f) = fixture().await?;
    let (runtime, agent, thread) = (f.runtime.clone(), f.agent.clone(), f.thread.clone());
    let send = tokio::spawn(async move { runtime.send(agent, thread, request()).await });
    let id = tokio::time::timeout(Duration::from_secs(5), f.started.recv())
        .await?
        .unwrap();
    f.runtime
        .suspend_turn(HarnessTurnKey::new(
            f.agent.record().id,
            f.thread.record().id,
            id,
        ))
        .await?;
    assert!(
        tokio::time::timeout(Duration::from_secs(5), send)
            .await??
            .unwrap_err()
            .to_string()
            .contains("suspended")
    );
    f.runtime.shutdown().await
}

#[tokio::test]
async fn unsupported_suspension_preserves_running_and_queued_controls() -> Result<()> {
    let (_temp, mut f) = fixture().await?;
    let mut config = f.runtime.get_agent_config(f.agent.as_ref()).await?;
    config.harness = crate::AgentHarnessKind::Rlm;
    let queue = f.state.turn_coordinator::<TurnWork>();
    let mut turns = Vec::new();
    for running in [true, false] {
        let (turn, stream) = f
            .runtime
            .start_turn(
                f.agent.clone(),
                f.thread.clone(),
                request(),
                false,
                Some(config.clone()),
                Default::default(),
            )
            .await?;
        if running {
            f.started(turn.id).await?;
        }
        assert!(!f.runtime.suspend_turn(f.key(&turn)).await?);
        assert_eq!(
            queue.get(f.scope(), turn.id).await?.unwrap().control,
            TurnControl::Run
        );
        turns.push((turn, stream));
    }
    for (index, (turn, stream)) in turns.into_iter().enumerate() {
        if index > 0 {
            f.started(turn.id).await?;
        }
        f.executor.release.add_permits(1);
        finish(stream).await?;
    }
    f.runtime.shutdown().await
}

#[tokio::test]
async fn resume_clears_a_suspension_waiting_for_a_tool_result() -> Result<()> {
    let (_temp, mut f) = fixture().await?;
    f.executor.tool_round.store(true, Ordering::SeqCst);
    let (turn, mut original) = f.start().await?;
    f.started(turn.id).await?;
    f.runtime.suspend_turn(f.key(&turn)).await?;
    let mut resumed = f.runtime.resume_turn(f.key(&turn)).await?;
    f.executor.release.add_permits(2);
    for stream in [&mut original, &mut resumed] {
        tokio::time::timeout(Duration::from_secs(5), async {
            while let Some(event) = stream.next().await {
                assert!(!matches!(event?, ExecutionStreamEvent::Suspended(_)));
            }
            Ok::<_, anyhow::Error>(())
        })
        .await??;
    }
    assert!(f.executor.resumed.lock().unwrap().is_empty());
    assert!(f.started.try_recv().is_err());
    assert!(matches!(
        journal_status(f.thread.as_ref(), &turn).await?,
        JournalStatus::Finished(Ok(_))
    ));
    f.runtime.shutdown().await
}
