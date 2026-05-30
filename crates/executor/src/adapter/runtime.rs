use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use exoharness::{AgentHandle, ConversationHandle, Secret};

use super::store::AdapterStore;
use super::types::{AdapterAttachment, AdapterConfig, AdapterEventType, AdapterRecord};
use super::worker::{WorkerCommand, WorkerEvent, run_worker_loop};
use crate::conversation_wakeup::send_conversation_wakeup;
use crate::{Harness, HarnessAgent, HarnessConversation};

#[derive(Debug, Clone, Copy)]
pub struct AdapterRunOptions {
    pub limit: usize,
}

impl Default for AdapterRunOptions {
    fn default() -> Self {
        Self { limit: 10 }
    }
}

pub async fn run_adapters_watch(
    harness: Arc<dyn Harness>,
    store: AdapterStore,
    options: AdapterRunOptions,
) -> Result<()> {
    let mut running = HashSet::new();
    loop {
        let adapters = store.enabled_adapters().await?;
        for adapter in adapters.into_iter().take(options.limit) {
            if !running.insert(adapter.id.clone()) {
                continue;
            }
            let harness = Arc::clone(&harness);
            let store = store.clone();
            tokio::spawn(async move {
                loop {
                    match store.get_adapter(&adapter.id).await {
                        Ok(Some(current)) if current.enabled => {}
                        Ok(Some(_)) => {
                            tokio::time::sleep(Duration::from_secs(5)).await;
                            continue;
                        }
                        Ok(_) => break,
                        Err(_) => break,
                    }
                    if let Err(error) =
                        run_adapter_loop(Arc::clone(&harness), &store, adapter.clone()).await
                    {
                        eprintln!(
                            "adapter {} runtime error: {error}; restarting in 5s",
                            adapter.id
                        );
                        let _ = store.mark_error(&adapter.id, error.to_string()).await;
                        let _ = store
                            .record_event(
                                adapter.id.clone(),
                                AdapterEventType::Error,
                                error.to_string(),
                            )
                            .await;
                    }
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            });
        }
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
}

pub async fn send_adapter_message_with_handles(
    _agent: &dyn AgentHandle,
    _conversation: &dyn ConversationHandle,
    store: &AdapterStore,
    adapter: &AdapterRecord,
    text: &str,
    target: Option<&str>,
    attachments: Vec<AdapterAttachment>,
) -> Result<()> {
    if !adapter.enabled {
        bail!("adapter is disabled: {}", adapter.id);
    }
    if !attachments.is_empty()
        && adapter.config.adapter_type != "whatsapp"
        && adapter.config.adapter_type != "signal"
    {
        bail!(
            "adapter {} does not support rich attachments",
            adapter.config.adapter_type
        );
    }
    // Note: we intentionally do not write a conversation artifact here.
    // This tool is invoked from inside an active agent turn, and writing to
    // the conversation outside the turn handle advances the conversation
    // head, which makes the active turn stale and crashes the adapter
    // worker. The outbound message is durably queued in the AdapterStore
    // outbox, and the event below records it for audit.
    store
        .record_event(
            adapter.id.clone(),
            AdapterEventType::Outbound,
            format!(
                "queued {} adapter message{}",
                adapter.config.adapter_type,
                target.map(|t| format!(" to {t}")).unwrap_or_default(),
            ),
        )
        .await?;
    store
        .enqueue_outbound_message(
            adapter.id.clone(),
            text.to_string(),
            target.map(ToOwned::to_owned),
            attachments,
        )
        .await?;
    Ok(())
}

async fn run_adapter_loop(
    harness: Arc<dyn Harness>,
    store: &AdapterStore,
    adapter: AdapterRecord,
) -> Result<()> {
    let agent = require_agent(harness.as_ref(), &adapter).await?;
    let conversation = require_conversation(agent.as_ref(), &adapter).await?;
    let config = &adapter.config;
    let secret_env = worker_secret_env(agent.exoharness_handle().as_ref(), config).await?;
    run_worker_loop(
        &adapter.id,
        config,
        secret_env,
        |event| {
            let store = store.clone();
            let adapter = adapter.clone();
            let conversation = std::sync::Arc::clone(&conversation);
            let config = config.clone();
            async move {
                handle_worker_event(&store, conversation.as_ref(), &adapter, &config, event).await
            }
        },
        || {
            let store = store.clone();
            let adapter_id = adapter.id.clone();
            async move {
                Ok(store
                    .take_outbound_messages(&adapter_id)
                    .await?
                    .into_iter()
                    .map(|message| WorkerCommand::SendMessage {
                        target: message.target,
                        text: message.text,
                        attachments: message.attachments,
                    })
                    .collect())
            }
        },
    )
    .await
}

async fn handle_worker_event(
    store: &AdapterStore,
    conversation: &dyn HarnessConversation,
    adapter: &AdapterRecord,
    config: &AdapterConfig,
    event: WorkerEvent,
) -> Result<()> {
    match event {
        WorkerEvent::Connected { subject, metadata } => {
            store.mark_connected(&adapter.id).await?;
            record_worker_lifecycle(
                store,
                conversation,
                adapter,
                config,
                "connected",
                serde_json::json!({ "subject": subject, "metadata": metadata }),
            )
            .await
        }
        WorkerEvent::Message {
            target,
            sender,
            text,
            message_id,
            metadata,
        } => {
            handle_worker_message(
                store,
                conversation,
                adapter,
                config,
                target,
                sender,
                text,
                message_id,
                metadata,
            )
            .await
        }
        WorkerEvent::Lifecycle { name, metadata } => {
            record_worker_lifecycle(store, conversation, adapter, config, &name, metadata).await
        }
        WorkerEvent::Error { message } => {
            store.mark_error(&adapter.id, message.clone()).await?;
            store
                .record_event(adapter.id.clone(), AdapterEventType::Error, message)
                .await?;
            Ok(())
        }
        WorkerEvent::Disconnected { reason } => {
            record_worker_lifecycle(
                store,
                conversation,
                adapter,
                config,
                "disconnected",
                serde_json::json!({ "reason": reason }),
            )
            .await
        }
    }
}

async fn handle_worker_message(
    store: &AdapterStore,
    conversation: &dyn HarnessConversation,
    adapter: &AdapterRecord,
    config: &AdapterConfig,
    target: String,
    sender: Option<String>,
    text: String,
    _message_id: Option<String>,
    _metadata: serde_json::Value,
) -> Result<()> {
    // Note: we intentionally do not write a conversation artifact here.
    // The wakeup turn below begins immediately, and any artifact writes
    // through the conversation handle (rather than the active turn) advance
    // the conversation head and could race with concurrent turns. The full
    // inbound text is delivered to the agent via the wakeup prompt and the
    // event is recorded in the AdapterStore for audit.
    store
        .record_event(
            adapter.id.clone(),
            AdapterEventType::Inbound,
            format!(
                "{} adapter message from {} to {}",
                config.adapter_type,
                sender.as_deref().unwrap_or("unknown"),
                target
            ),
        )
        .await?;
    send_conversation_wakeup(
        conversation,
        format!(
            "{} message received at target `{}` from {} via adapter `{}`:\n\n{}\n\nUse send_adapter_message with adapterId `{}` and target `{}` if you should reply externally. If this asks you to schedule future work whose results should be posted back externally, include this adapterId and target in the scheduled task reportPrompt.",
            config.adapter_type,
            target,
            sender.as_deref().unwrap_or("unknown"),
            adapter.name,
            text,
            adapter.id,
            target,
        ),
    )
    .await?;
    Ok(())
}

async fn record_worker_lifecycle(
    store: &AdapterStore,
    _conversation: &dyn HarnessConversation,
    adapter: &AdapterRecord,
    config: &AdapterConfig,
    event_type: &str,
    _payload: serde_json::Value,
) -> Result<()> {
    // Note: lifecycle events are recorded only in the AdapterStore. Writing
    // them to the conversation as artifacts can advance the head outside of
    // any active turn and corrupt the agent's turn state.
    store
        .record_event(
            adapter.id.clone(),
            match event_type {
                "connected" => AdapterEventType::Connected,
                "error" => AdapterEventType::Error,
                _ => AdapterEventType::Inbound,
            },
            format!("{} worker {event_type}", config.adapter_type),
        )
        .await?;
    Ok(())
}

async fn require_agent(
    harness: &dyn Harness,
    adapter: &AdapterRecord,
) -> Result<Arc<dyn HarnessAgent>> {
    harness
        .get_agent(&adapter.agent_id)
        .await?
        .ok_or_else(|| anyhow!("adapter agent does not exist: {}", adapter.agent_id))
}

async fn require_conversation(
    agent: &dyn HarnessAgent,
    adapter: &AdapterRecord,
) -> Result<Arc<dyn HarnessConversation>> {
    agent
        .get_conversation(&adapter.conversation_id)
        .await?
        .ok_or_else(|| {
            anyhow!(
                "adapter conversation does not exist: {}",
                adapter.conversation_id
            )
        })
}

async fn worker_secret_env(
    agent: &dyn AgentHandle,
    config: &AdapterConfig,
) -> Result<Vec<(String, String)>> {
    let mut env = Vec::new();
    for secret_env in &config.secret_env {
        let secret_uuid = secret_env.secret_id.parse()?;
        let Some(secret) = agent.get_secret(&secret_uuid).await? else {
            bail!("adapter secret not found: {}", secret_env.secret_id);
        };
        let value = match secret {
            Secret::Key { value } => value,
            Secret::Oauth { .. } => bail!("adapter worker secrets must be key secrets"),
        };
        env.push((secret_env.env.clone(), value));
    }
    Ok(env)
}
