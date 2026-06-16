use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use exoharness::{AgentHandle, ConversationHandle, Secret, SecretId};
use tokio::sync::Notify;

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
                        tracing::error!(
                            adapter_id = %adapter.id,
                            %error,
                            "adapter runtime error; restarting in 5s"
                        );
                        if let Err(mark_error) =
                            store.mark_error(&adapter.id, error.to_string()).await
                        {
                            tracing::error!(
                                adapter_id = %adapter.id,
                                error = %mark_error,
                                "failed to mark adapter error"
                            );
                        }
                        if let Err(record_error) = store
                            .record_event(
                                adapter.id.clone(),
                                AdapterEventType::Error,
                                error.to_string(),
                            )
                            .await
                        {
                            tracing::error!(
                                adapter_id = %adapter.id,
                                error = %record_error,
                                "failed to record adapter error"
                            );
                        }
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
        && adapter.config.adapter_type != "discord"
    {
        bail!(
            "adapter {} does not support rich attachments",
            adapter.config.adapter_type
        );
    }
    // Note: we intentionally do not write a conversation artifact here.
    // The outbound message is durably queued in the AdapterStore outbox, and
    // the event below records it for audit without adding adapter bookkeeping
    // to the agent-visible conversation history.
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
    notify_adapter_outbound(&adapter.id);
    Ok(())
}

async fn run_adapter_loop(
    harness: Arc<dyn Harness>,
    store: &AdapterStore,
    adapter: AdapterRecord,
) -> Result<()> {
    let agent = require_agent(harness.as_ref(), &adapter).await?;
    let conversation = require_conversation(agent.as_ref(), &adapter).await?;
    store.requeue_inflight_messages(&adapter.id).await?;
    let config = adapter.config.clone();
    let secret_env = worker_secret_env(agent.exoharness_handle().as_ref(), &config).await?;
    let outbound_notifier = register_adapter_outbound_notifier(&adapter.id);
    let event_store = store.clone();
    let event_adapter = adapter.clone();
    let event_conversation = std::sync::Arc::clone(&conversation);
    let event_config = config.clone();
    let outbound_store = store.clone();
    let outbound_adapter_id = adapter.id.clone();
    let stop_store = store.clone();
    let stop_adapter_id = adapter.id.clone();
    run_worker_loop(
        &adapter.id,
        &config,
        secret_env,
        Arc::clone(&outbound_notifier.notify),
        move |event| {
            let store = event_store.clone();
            let adapter = event_adapter.clone();
            let conversation = std::sync::Arc::clone(&event_conversation);
            let config = event_config.clone();
            async move {
                handle_worker_event(&store, conversation.as_ref(), &adapter, &config, event).await
            }
        },
        move || {
            let store = outbound_store.clone();
            let adapter_id = outbound_adapter_id.clone();
            async move {
                Ok(store
                    .claim_outbound_messages(&adapter_id)
                    .await?
                    .into_iter()
                    .map(|message| WorkerCommand::SendMessage {
                        id: message.id,
                        target: message.target,
                        text: message.text,
                        attachments: message.attachments,
                    })
                    .collect())
            }
        },
        move || {
            let store = stop_store.clone();
            let adapter_id = stop_adapter_id.clone();
            async move {
                Ok(store
                    .get_adapter(&adapter_id)
                    .await?
                    .is_none_or(|adapter| !adapter.enabled))
            }
        },
    )
    .await
}

struct AdapterOutboundNotifierGuard {
    adapter_id: String,
    notify: Arc<Notify>,
}

impl Drop for AdapterOutboundNotifierGuard {
    fn drop(&mut self) {
        let mut notifiers = adapter_outbound_notifiers()
            .lock()
            .expect("adapter outbound notifier registry poisoned");
        if let Some(current) = notifiers.get(&self.adapter_id).and_then(Weak::upgrade)
            && Arc::ptr_eq(&current, &self.notify)
        {
            notifiers.remove(&self.adapter_id);
        }
    }
}

fn register_adapter_outbound_notifier(adapter_id: &str) -> AdapterOutboundNotifierGuard {
    let notify = Arc::new(Notify::new());
    adapter_outbound_notifiers()
        .lock()
        .expect("adapter outbound notifier registry poisoned")
        .insert(adapter_id.to_string(), Arc::downgrade(&notify));
    AdapterOutboundNotifierGuard {
        adapter_id: adapter_id.to_string(),
        notify,
    }
}

fn notify_adapter_outbound(adapter_id: &str) {
    let notify = adapter_outbound_notifiers()
        .lock()
        .expect("adapter outbound notifier registry poisoned")
        .get(adapter_id)
        .and_then(Weak::upgrade);
    if let Some(notify) = notify {
        notify.notify_one();
    }
}

fn adapter_outbound_notifiers() -> &'static Mutex<HashMap<String, Weak<Notify>>> {
    static NOTIFIERS: OnceLock<Mutex<HashMap<String, Weak<Notify>>>> = OnceLock::new();
    NOTIFIERS.get_or_init(|| Mutex::new(HashMap::new()))
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
        WorkerEvent::CommandAck { command_id } => {
            store
                .acknowledge_outbound_message(&adapter.id, &command_id)
                .await
        }
        WorkerEvent::CommandNack {
            command_id,
            message,
        } => {
            store.mark_error(&adapter.id, message.clone()).await?;
            store
                .record_event(adapter.id.clone(), AdapterEventType::Error, message)
                .await?;
            store
                .acknowledge_outbound_message(&adapter.id, &command_id)
                .await
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
    message_id: Option<String>,
    _metadata: serde_json::Value,
) -> Result<()> {
    if let Some(message_id) = &message_id
        && !store
            .record_inbound_message_once(&adapter.id, &target, message_id)
            .await?
    {
        return Ok(());
    }
    // Note: we intentionally do not write a conversation artifact here.
    // The full inbound text is delivered to the agent via the wakeup prompt,
    // and the event is recorded in the AdapterStore for audit without adding
    // adapter bookkeeping to the agent-visible conversation history.
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
            "{} message received at target `{}` from {} via adapter `{}`:\n\n{}\n\nThis message came from an external adapter. If you answer this message, you MUST reply externally with send_adapter_message using adapterId `{}` and target `{}`. Do not answer only in the REPL unless you are explicitly deciding that no external reply should be sent. If this asks you to schedule future work whose results should be posted back externally, include adapterId `{}` and target `{}` in the scheduled task reportPrompt.",
            config.adapter_type,
            target,
            sender.as_deref().unwrap_or("unknown"),
            adapter.name,
            text,
            adapter.id,
            target,
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
    // Note: lifecycle events are recorded only in the AdapterStore so adapter
    // bookkeeping does not pollute the agent-visible conversation history.
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
        let secret_uuid = resolve_secret_id(agent, &secret_env.secret_id).await?;
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

async fn resolve_secret_id(agent: &dyn AgentHandle, reference: &str) -> Result<SecretId> {
    if let Ok(secret_id) = reference.parse() {
        return Ok(secret_id);
    }
    agent
        .list_secrets()
        .await?
        .into_iter()
        .find(|secret| secret.name == reference)
        .map(|secret| secret.id)
        .ok_or_else(|| anyhow!("adapter secret not found: {reference}"))
}
