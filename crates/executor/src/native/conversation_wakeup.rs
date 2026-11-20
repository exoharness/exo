//! Wakeups: a producer outside a conversation asking it to run a turn.
//!
//! An adapter socket, a scheduler fire, or the CLI waking a conversation all go
//! through [`send_conversation_wakeup`]. A wakeup is an ordinary turn: it is
//! admitted through the provider's turn queue, which owns FIFO ordering,
//! duplicate suppression, and per-thread exclusivity via a `TurnLease`. Two
//! wakeups racing one conversation therefore queue, run one after another, and
//! the later one is accepted while the earlier one is still running.
//!
//! That is why this module holds no lock. It used to: a `/tmp` file per
//! conversation, polled at 100ms, that serialized wakeups within one host and
//! deleted any lock older than 30 minutes. The queue does the same job in the
//! substrate, and it does it across processes, which the file could not — a
//! container's temp directory is private to it. The stale timeout also failed
//! open exactly when it mattered: a turn legitimately running longer than 30
//! minutes had its guard stolen out from under it.
//!
//! Durability is a host's decision, not something a wakeup re-implements. A host
//! that must survive restarts supplies a persistent coordinator through
//! `LocalProvider::with_turn_coordinator`, and re-wakes a thread it already
//! knows about with `LocalProvider::wake_turn_queue`, as a Durable Object alarm
//! would.

use std::sync::Arc;

use exoharness::Result;
use lingua::Message;
use lingua::universal::UserContent;

use crate::{AgentHandle, ConversationHandle, Runtime, SendRequest, SendResult};

pub async fn send_conversation_wakeup(
    harness: &Runtime,
    agent: &Arc<dyn AgentHandle>,
    conversation: &Arc<dyn ConversationHandle>,
    prompt: String,
) -> Result<SendResult> {
    send_conversation_wakeup_content(harness, agent, conversation, UserContent::String(prompt))
        .await
}

/// Wakeup variant for multimodal content, e.g. adapter messages that carry
/// inbound images for the model to analyze.
pub async fn send_conversation_wakeup_content(
    harness: &Runtime,
    agent: &Arc<dyn AgentHandle>,
    conversation: &Arc<dyn ConversationHandle>,
    content: UserContent,
) -> Result<SendResult> {
    let result = harness
        .send(
            Arc::clone(agent),
            Arc::clone(conversation),
            SendRequest {
                input: vec![Message::User { content }],
                session_id: None,
            },
        )
        .await?;
    conversation.end_session(result.session_id).await?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use anyhow::Result;
    use async_trait::async_trait;
    use exoharness::{BasicExoHarness, ConversationHandle};
    use lingua::Message;
    use lingua::universal::AssistantContent;
    use tempfile::TempDir;
    use tokio::sync::{Notify, Semaphore};

    use crate::test_support::{create_test_credential, local_test_config};
    use crate::{
        AgentHarnessKind, BasicToolRuntime, CreateAgentRequest, CreateConversationRequest,
        LocalProvider, ModelClient, ModelRequest, ModelResponse, ModelResponseStream,
    };

    use super::*;

    /// A model client whose completions park until released, so a test can hold
    /// one turn open while a second wakeup arrives, and can observe whether the
    /// two turns ever ran at the same time.
    struct GatedModelClient {
        gate: Semaphore,
        started: Notify,
        running: AtomicUsize,
        max_running: AtomicUsize,
    }

    impl GatedModelClient {
        fn new() -> Self {
            Self {
                gate: Semaphore::new(0),
                started: Notify::new(),
                running: AtomicUsize::new(0),
                max_running: AtomicUsize::new(0),
            }
        }

        fn release(&self, permits: usize) {
            self.gate.add_permits(permits);
        }

        async fn wait_until_running(&self) {
            tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    // Register the waiter before checking, so an immediate
                    // completion cannot be missed.
                    let notified = self.started.notified();
                    if self.running.load(Ordering::SeqCst) > 0 {
                        return;
                    }
                    notified.await;
                }
            })
            .await
            .expect("the first wakeup never reached the model");
        }

        fn max_running(&self) -> usize {
            self.max_running.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl ModelClient for GatedModelClient {
        async fn complete(&self, _: ModelRequest) -> Result<ModelResponse> {
            let running = self.running.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_running.fetch_max(running, Ordering::SeqCst);
            self.started.notify_waiters();
            self.gate
                .acquire()
                .await
                .expect("the gate is never closed")
                .forget();
            self.running.fetch_sub(1, Ordering::SeqCst);
            Ok(ModelResponse {
                provider_cost_usd: None,
                response_id: None,
                messages: vec![Message::Assistant {
                    id: None,
                    content: AssistantContent::String("woke".to_string()),
                }],
                tool_calls: Vec::new(),
                usage: None,
                model: None,
                ttft: None,
                duration: None,
            })
        }

        async fn complete_stream(&self, _: ModelRequest) -> Result<Box<dyn ModelResponseStream>> {
            anyhow::bail!("wakeups do not stream")
        }
    }

    #[tokio::test]
    async fn concurrent_wakeups_run_one_turn_at_a_time() -> Result<()> {
        let tempdir = TempDir::new()?;
        let root = tempdir.path().join("exoharness");
        let state = Arc::new(BasicExoHarness::new(local_test_config(&root)).await?);
        let client = Arc::new(GatedModelClient::new());
        let runtime = Runtime::new(
            LocalProvider::basic(
                state.clone(),
                Arc::clone(&client),
                Arc::new(BasicToolRuntime),
                Arc::new(cost::PricingTable::empty()),
            )
            .with_turn_coordinator(state.turn_coordinator()),
            None,
        );
        create_test_credential(state.as_ref()).await;
        let agent = runtime
            .create_agent(CreateAgentRequest {
                ..crate::test_support::agent_request("wakeup", AgentHarnessKind::Basic)
            })
            .await?;
        let conversation: Arc<dyn ConversationHandle> = runtime
            .create_conversation(agent.as_ref(), CreateConversationRequest::default())
            .await?;

        let wake = |prompt: &'static str| {
            let runtime = runtime.clone();
            let agent = Arc::clone(&agent);
            let conversation = Arc::clone(&conversation);
            async move {
                send_conversation_wakeup(&runtime, &agent, &conversation, prompt.to_string()).await
            }
        };

        // First wakeup runs its turn and parks in the model.
        let first = tokio::spawn(wake("first"));
        client.wait_until_running().await;

        // Second wakeup arrives while the first turn is still open.
        let second = tokio::spawn(wake("second"));
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Let both through. If the turns were allowed to overlap, the second
        // would have entered the model while the first was parked here.
        client.release(2);
        let first = tokio::time::timeout(Duration::from_secs(30), first).await???;
        let second = tokio::time::timeout(Duration::from_secs(30), second).await???;

        assert_ne!(
            first.turn_id, second.turn_id,
            "each wakeup must be its own turn; neither may be deduped away"
        );
        assert_eq!(
            client.max_running(),
            1,
            "wakeups of one conversation must not execute turns concurrently"
        );

        // Serialized turns also leave a non-interleaved log: every event of one
        // turn precedes every event of the next.
        let events = conversation.get_events(None).await?.events;
        let runs = events.iter().filter_map(|event| event.turn_id).fold(
            Vec::new(),
            |mut runs: Vec<_>, turn| {
                if runs.last() != Some(&turn) {
                    runs.push(turn);
                }
                runs
            },
        );
        assert_eq!(
            runs.len(),
            2,
            "the two wakeups' events interleaved in the conversation log"
        );

        runtime.shutdown().await?;
        Ok(())
    }
}
